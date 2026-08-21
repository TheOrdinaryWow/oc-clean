use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, info_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;

use super::AnalysisReport;
use crate::analyze::{attribution, distribution, orphans, space};
use crate::cli::{AnalyzeArgs, Cli};
use crate::db::{self, ConnectionOptions, DatabaseConnection};
use crate::error::Error;
use crate::paths::{self, DatabaseOptions, DerivedPaths, Environment, Platform, Target};

/// Runs read-only analysis and writes the selected report format to `output`.
///
/// # Errors
///
/// Returns the existing typed database, schema, path, analysis, or output error.
pub fn run(cli: &Cli, arguments: &AnalyzeArgs, output: &mut dyn Write) -> Result<(), Error> {
    let environment = Environment::from_iter(std::env::vars_os());
    let target = paths::database_target(
        &environment,
        DatabaseOptions {
            explicit: cli.db.as_deref(),
            channel: cli.channel.as_deref(),
            platform: current_platform()?,
        },
    )?;
    let derived_paths = report_paths(&target);
    let database = db::open_read_only(&target, ConnectionOptions::default())?;
    db::schema::inspect(database.connection(), cli.force_schema)?;

    let report = if arguments.quick {
        quick_report(&database, &target)?
    } else {
        full_report(&database, &derived_paths, arguments.top)?
    };
    info!(mode = ?report.mode, "analysis complete");

    if arguments.json {
        super::write_json(&report, output)
    } else {
        let color = super::human::color_enabled(
            io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        );
        super::write_human(&report, output, color)
    }
}

fn full_report<Access>(
    database: &DatabaseConnection<Access>,
    paths: &DerivedPaths,
    top: usize,
) -> Result<AnalysisReport, Error> {
    let progress = info_span!("analyze", "indicatif.pb_show" = true);
    progress.pb_set_length(5);
    let _entered = progress.enter();

    progress.pb_set_message("counting rows");
    let row_counts = row_counts(database.connection())?;
    progress.pb_inc(1);
    progress.pb_set_message("accounting for SQLite space");
    let space = space::analyze(database)?;
    progress.pb_inc(1);
    progress.pb_set_message("attributing project and session bytes");
    let attribution = attribution::analyze(database, top)?;
    progress.pb_inc(1);
    progress.pb_set_message("counting orphans");
    let orphans = orphans::analyze(database, paths)?;
    progress.pb_inc(1);
    progress.pb_set_message("building age and directory overview");
    let distribution = distribution::analyze(database, paths, now_ms()?)?;
    progress.pb_inc(1);
    progress.pb_set_finish_message("analysis complete");

    Ok(AnalysisReport::full(
        space,
        row_counts,
        attribution,
        orphans,
        distribution,
    ))
}

fn quick_report<Access>(
    database: &DatabaseConnection<Access>,
    target: &Target,
) -> Result<AnalysisReport, Error> {
    let file_space = file_space(database.connection(), target)?;
    let row_counts = row_counts(database.connection())?;
    Ok(AnalysisReport::quick(file_space, row_counts))
}

fn file_space(
    connection: &rusqlite::Connection,
    target: &Target,
) -> Result<space::FileSpace, Error> {
    let database_path = target_path(target);
    let page_count = pragma_u32(connection, "page_count")?;
    let freelist_count = pragma_u32(connection, "freelist_count")?;
    let page_size = pragma_u32(connection, "page_size")?;
    let total_bytes = u64::from(page_count) * u64::from(page_size);
    let freelist_bytes = u64::from(freelist_count) * u64::from(page_size);
    let live_bytes = total_bytes.saturating_sub(freelist_bytes);
    let freelist_percent = if page_count == 0 {
        0.0
    } else {
        f64::from(freelist_count) * 100.0 / f64::from(page_count)
    };

    let (wal_bytes, shm_bytes) = match target {
        Target::File(_) => (
            sidecar_bytes(database_path, "-wal")?,
            sidecar_bytes(database_path, "-shm")?,
        ),
        Target::Memory => (None, None),
    };

    Ok(space::FileSpace {
        page_count,
        freelist_count,
        page_size,
        total_bytes,
        live_bytes,
        freelist_bytes,
        freelist_percent,
        wal_bytes,
        shm_bytes,
    })
}

fn row_counts(connection: &rusqlite::Connection) -> Result<BTreeMap<String, u64>, Error> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .map_err(|source| sqlite_error("preparing table enumeration for row counts", source))?;
    let table_names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| sqlite_error("querying table names for row counts", source))?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|source| sqlite_error("reading table names for row counts", source))?;
    let mut counts = BTreeMap::new();
    for table in table_names {
        let quoted = table.replace('"', "\"\"");
        let sql = format!("SELECT COUNT(*) FROM \"{quoted}\"");
        let count = connection
            .query_row(&sql, [], |row| row.get::<_, i64>(0))
            .map_err(|source| sqlite_error(&format!("counting rows in `{table}`"), source))?;
        let count = u64::try_from(count).map_err(|_| {
            sqlite_error(
                &format!("reading row count for `{table}`"),
                integral_error(count),
            )
        })?;
        counts.insert(table, count);
    }
    Ok(counts)
}

/// Placeholder data directory for an in-memory database, which owns no external storage.
///
/// The name must stay a syntactically valid relative path: Windows rejects `:memory:\\storage`
/// outright instead of reporting it as absent, which turns "no external storage" into an error.
const MEMORY_DATA_DIR: &str = "oc-clean-memory-target";

fn report_paths(target: &Target) -> DerivedPaths {
    if matches!(target, Target::Memory) {
        return paths::derived_paths(Path::new(MEMORY_DATA_DIR));
    }
    let path = target_path(target);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    paths::derived_paths(parent)
}

fn target_path(target: &Target) -> &Path {
    match target {
        Target::File(path) => path,
        Target::Memory => Path::new(":memory:"),
    }
}

fn pragma_u32(connection: &rusqlite::Connection, name: &str) -> Result<u32, Error> {
    connection
        .pragma_query_value(None, name, |row| row.get(0))
        .map_err(|source| sqlite_error(&format!("reading PRAGMA {name}"), source))
}

fn sidecar_bytes(database_path: &Path, suffix: &str) -> Result<Option<u64>, Error> {
    let mut path = OsString::from(database_path.as_os_str());
    path.push(suffix);
    let path = PathBuf::from(path);
    match path.metadata() {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io { path, source }),
    }
}

fn now_ms() -> Result<i64, Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| Error::InvalidArgument {
            argument: "system clock".to_owned(),
            reason: source.to_string(),
        })?;
    i64::try_from(elapsed.as_millis()).map_err(|_| Error::InvalidArgument {
        argument: "system clock".to_owned(),
        reason: "millisecond epoch exceeds SQLite's signed integer range".to_owned(),
    })
}

fn current_platform() -> Result<Platform, Error> {
    match std::env::consts::OS {
        "linux" => Ok(Platform::Linux),
        "macos" => Ok(Platform::MacOs),
        "windows" => Ok(Platform::Windows),
        platform => Err(Error::UnsupportedPlatform {
            platform: platform.to_owned(),
        }),
    }
}

fn integral_error(value: i64) -> rusqlite::Error {
    rusqlite::Error::IntegralValueOutOfRange(0, value)
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    db::sqlite_error(context, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_locked_maps_to_exit_five() {
        let source = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        );

        assert_eq!(sqlite_error("testing report query", source).exit_code(), 5);
    }
}
