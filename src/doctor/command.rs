use std::io::{IsTerminal, Write};
use std::path::Path;

use super::model::{CheckReport, DoctorReport, ForeignKeyReport, HolderReport, VacuumHeadroom};
use crate::analyze::space::FileSpace;
use crate::analyze::{orphans, space};
use crate::cli::{Cli, DoctorArgs};
use crate::db::{self, ConnectionOptions};
use crate::error::Error;
use crate::parallel::{self, JobHandle};
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
    let report = inspect(
        &target,
        database_path,
        &derived_paths,
        run_independent_checks,
    )?;

    if arguments.json {
        super::json::write(&report, output)?;
    } else {
        let style = crate::report::format::Style::resolve(
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        );
        super::human::write(&report, output, style)?;
    }
    ensure_report_health(&report)
}

fn inspect(
    target: &Target,
    database_path: &Path,
    derived_paths: &paths::DerivedPaths,
    independent_checks: impl FnOnce(&Target, &Path) -> Result<IndependentChecks, Error>,
) -> Result<DoctorReport, Error> {
    let database = db::open_read_only(target, ConnectionOptions::default())?;
    let connection = database.connection();

    let bar = progress::spinner("doctor", "inspecting the schema");
    let schema = db::schema::inspect_report(connection)?;
    bar.finish();
    ensure_schema_compatible(&schema)?;

    let IndependentChecks {
        integrity_check,
        foreign_key_check,
        file_space,
        holders,
    } = independent_checks(target, database_path)?;

    let bar = progress::spinner("doctor", "counting orphans");
    let orphans = orphans::analyze(&database, derived_paths)?;
    bar.finish();

    let bar = progress::spinner("doctor", "estimating rebuild headroom");
    let vacuum_headroom = vacuum_headroom(
        target,
        database_path,
        file_space.live_bytes,
        database.capabilities().hard_links,
    )?;
    bar.finish();

    let bar = progress::spinner("doctor", "reading auto-vacuum and timestamp state");
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
    bar.finish();
    Ok(report)
}

#[derive(Debug, PartialEq)]
struct IndependentChecks {
    integrity_check: CheckReport,
    foreign_key_check: ForeignKeyReport,
    file_space: FileSpace,
    holders: HolderReport,
}

fn run_independent_checks(
    target: &Target,
    database_path: &Path,
) -> Result<IndependentChecks, Error> {
    parallel::group("doctor", 4, |group| {
        let integrity_check = group.spawn("running the integrity check", || {
            let database = db::open_read_only(target, ConnectionOptions::default())?;
            super::checks::integrity_check(database.connection())
        });
        let foreign_key_check = group.spawn("running the foreign-key check", || {
            let database = db::open_read_only(target, ConnectionOptions::default())?;
            super::checks::foreign_key_check(database.connection())
        });
        let file_space = group.spawn("accounting for file space", || {
            let database = db::open_read_only(target, ConnectionOptions::default())?;
            Ok(space::analyze(&database)?.file)
        });
        let holders = group.spawn("scanning for database holders", || {
            Ok(super::checks::holders(database_path))
        });

        join_independent_checks(integrity_check, foreign_key_check, file_space, holders)
    })
}

fn join_independent_checks(
    integrity_check: JobHandle<'_, CheckReport>,
    foreign_key_check: JobHandle<'_, ForeignKeyReport>,
    file_space: JobHandle<'_, FileSpace>,
    holders: JobHandle<'_, HolderReport>,
) -> Result<IndependentChecks, Error> {
    // Joining every handle before propagating errors lets scoped workers finish while preserving
    // declaration order as the stable error-priority contract.
    let integrity_check = integrity_check.join();
    let foreign_key_check = foreign_key_check.join();
    let file_space = file_space.join();
    let holders = holders.join();

    Ok(IndependentChecks {
        integrity_check: integrity_check?,
        foreign_key_check: foreign_key_check?,
        file_space: file_space?,
        holders: holders?,
    })
}

#[cfg(test)]
fn run_independent_checks_sequential(
    target: &Target,
    database_path: &Path,
) -> Result<IndependentChecks, Error> {
    let database = db::open_read_only(target, ConnectionOptions::default())?;
    Ok(IndependentChecks {
        integrity_check: super::checks::integrity_check(database.connection())?,
        foreign_key_check: super::checks::foreign_key_check(database.connection())?,
        file_space: space::analyze(&database)?.file,
        holders: super::checks::holders(database_path),
    })
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

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
