use std::io::Write;
use std::path::Path;

use super::model::{DoctorReport, VacuumHeadroom};
use crate::analyze::{orphans, space};
use crate::cli::{Cli, DoctorArgs};
use crate::db::{self, ConnectionOptions};
use crate::error::Error;
use crate::paths::{self, DatabaseOptions, Environment, Platform, Target};
use crate::report::progress;

/// Runs the read-only diagnostics and writes the selected report format.
///
/// # Errors
///
/// Returns a typed path, SQLite, integrity, analysis, or output error.
pub fn run(cli: &Cli, arguments: &DoctorArgs, output: &mut dyn Write) -> Result<(), Error> {
    let target = database_target(cli)?;
    let database_path = target_path(&target);
    let data_directory = database_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new(paths::MEMORY_DATA_DIR));
    let derived_paths = paths::derived_paths(data_directory);
    let database = db::open_read_only(&target, ConnectionOptions::default())?;
    let connection = database.connection();

    // An integrity check on a large database runs for minutes with no output of its own,
    // so every step is announced; a silent terminal is indistinguishable from a hang.
    let bar = progress::phases("doctor", 8);
    bar.set_message("running the integrity check");
    let integrity_check = super::checks::integrity_check(connection)?;
    bar.step("inspecting the schema");
    let schema = db::schema::inspect_report(connection)?;
    ensure_schema_compatible(&schema)?;
    bar.step("running the foreign-key check");
    let foreign_key_check = super::checks::foreign_key_check(connection)?;
    bar.step("accounting for file space");
    let file_space = space::analyze(&database)?.file;
    bar.step("counting orphans");
    let orphans = orphans::analyze(&database, &derived_paths)?;
    bar.step("scanning for database holders");
    let holders = super::checks::holders(database_path);
    bar.step("estimating rebuild headroom");
    let vacuum_headroom = vacuum_headroom(
        &target,
        database_path,
        file_space.live_bytes,
        database.capabilities().hard_links,
    )?;
    bar.step("reading auto-vacuum and timestamp state");
    let report = DoctorReport {
        database_path: database_path.to_owned(),
        schema,
        integrity_check,
        foreign_key_check,
        orphans,
        holders,
        vacuum_headroom,
        auto_vacuum: super::checks::auto_vacuum(connection)?,
        timestamp_sanity: super::checks::timestamp_sanity(connection)?,
    };
    bar.step("writing the report");
    bar.finish();

    if arguments.json {
        super::json::write(&report, output)?;
    } else {
        super::human::write(&report, output)?;
    }
    ensure_report_health(&report)
}

fn ensure_schema_compatible(schema: &db::schema::SchemaReport) -> Result<(), Error> {
    if schema.tier_one_missing.is_empty() {
        Ok(())
    } else {
        Err(Error::SchemaIncompatible {
            incompatibility: schema.tier_one_missing.join("; "),
        })
    }
}

fn ensure_report_health(report: &DoctorReport) -> Result<(), Error> {
    if !report.integrity_check.ok {
        return Err(Error::IntegrityCheckFailed {
            check: "integrity_check".to_owned(),
            message: report.integrity_check.findings.join("; "),
        });
    }
    if !report.foreign_key_check.ok {
        return Err(Error::IntegrityCheckFailed {
            check: "foreign_key_check".to_owned(),
            message: format!(
                "{} foreign key violation(s)",
                report.foreign_key_check.findings.len()
            ),
        });
    }
    if !report.timestamp_sanity.ok {
        return Err(Error::IntegrityCheckFailed {
            check: "timestamp-unit sanity probe".to_owned(),
            message: report.timestamp_sanity.detail.clone(),
        });
    }
    Ok(())
}

fn database_target(cli: &Cli) -> Result<Target, Error> {
    let environment = Environment::from_iter(std::env::vars_os());
    paths::database_target(
        &environment,
        DatabaseOptions {
            explicit: cli.db.as_deref(),
            channel: cli.channel.as_deref(),
            platform: current_platform()?,
        },
    )
}

fn target_path(target: &Target) -> &Path {
    match target {
        Target::File(path) => path,
        Target::Memory => Path::new(":memory:"),
    }
}

fn vacuum_headroom(
    target: &Target,
    database_path: &Path,
    current_live_bytes: u64,
    hardlink_supported: bool,
) -> Result<VacuumHeadroom, Error> {
    match target {
        Target::File(_) => {
            super::checks::vacuum_headroom(database_path, current_live_bytes, hardlink_supported)
        }
        Target::Memory => Ok(VacuumHeadroom {
            estimated_required_bytes: 0,
            available_bytes: 0,
            vacuum_into_feasible: true,
        }),
    }
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
