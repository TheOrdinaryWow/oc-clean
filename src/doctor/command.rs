use std::io::Write;
use std::path::Path;

use super::model::DoctorReport;
use crate::analyze::{orphans, space};
use crate::cli::{Cli, DoctorArgs};
use crate::db::{self, ConnectionOptions};
use crate::error::Error;
use crate::paths::{self, DatabaseOptions, Environment, Platform, Target};

/// Runs the read-only diagnostics and writes the selected report format.
///
/// # Errors
///
/// Returns a typed path, SQLite, integrity, analysis, or output error.
pub fn run(cli: &Cli, arguments: &DoctorArgs, output: &mut dyn Write) -> Result<(), Error> {
    let target = database_target(cli)?;
    let database_path = file_path(&target)?;
    let data_directory = database_path
        .parent()
        .ok_or_else(|| Error::InvalidArgument {
            argument: "--db".to_owned(),
            reason: "database path must have a parent directory".to_owned(),
        })?;
    let derived_paths = paths::derived_paths(data_directory);
    let database = db::open_read_only(&target, ConnectionOptions::default())?;
    let connection = database.connection();

    let integrity_check = super::checks::integrity_check(connection)?;
    let schema = db::schema::inspect_report(connection)?;
    ensure_schema_compatible(&schema)?;
    let foreign_key_check = super::checks::foreign_key_check(connection)?;
    let file_space = space::analyze(&database)?.file;
    let report = DoctorReport {
        database_path: database_path.to_owned(),
        schema,
        integrity_check,
        foreign_key_check,
        orphans: orphans::analyze(&database, &derived_paths)?,
        holders: super::checks::holders(database_path),
        vacuum_headroom: super::checks::vacuum_headroom(
            database_path,
            file_space.live_bytes,
            database.capabilities().hard_links,
        )?,
        auto_vacuum: super::checks::auto_vacuum(connection)?,
        timestamp_sanity: super::checks::timestamp_sanity(connection)?,
    };

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
            channel: None,
            platform: current_platform()?,
        },
    )
}

fn file_path(target: &Target) -> Result<&Path, Error> {
    match target {
        Target::File(path) => Ok(path),
        Target::Memory => Err(Error::InvalidArgument {
            argument: "--db".to_owned(),
            reason: "doctor requires a file-backed database".to_owned(),
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
