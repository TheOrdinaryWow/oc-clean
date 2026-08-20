use std::path::Path;

use rusqlite::Connection;

use super::model::{
    AutoVacuumReport, CheckReport, ForeignKeyFinding, ForeignKeyReport, HolderProcess,
    HolderReport, TimestampSanity, VacuumHeadroom,
};
use crate::error::Error;
use crate::safety::holders::{
    CommandMode, Completeness, HolderInspector, Inspection, Verdict, inspect_and_decide,
};

const YEAR_2020_SECONDS: i64 = 1_577_836_800;
const YEAR_2100_SECONDS: i64 = 4_102_444_800;
const YEAR_2020_MILLISECONDS: i64 = YEAR_2020_SECONDS * 1_000;
const YEAR_2100_MILLISECONDS: i64 = YEAR_2100_SECONDS * 1_000;

pub(super) fn integrity_check(connection: &Connection) -> Result<CheckReport, Error> {
    let findings = string_rows(
        connection,
        "PRAGMA integrity_check",
        "running PRAGMA integrity_check",
    )
    .map_err(|error| integrity_error("integrity_check", &error))?;
    let ok = findings.as_slice() == ["ok"];
    Ok(CheckReport { ok, findings })
}

pub(super) fn foreign_key_check(connection: &Connection) -> Result<ForeignKeyReport, Error> {
    let mut statement = connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(|source| sqlite_error("preparing PRAGMA foreign_key_check", source))?;
    let rows = statement
        .query_map([], |row| {
            Ok(ForeignKeyFinding {
                table: row.get(0)?,
                row_id: row.get(1)?,
                parent_table: row.get(2)?,
                foreign_key_index: row.get(3)?,
            })
        })
        .map_err(|source| sqlite_error("running PRAGMA foreign_key_check", source))?;
    let findings = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error("reading PRAGMA foreign_key_check", source))?;
    Ok(ForeignKeyReport {
        ok: findings.is_empty(),
        findings,
    })
}

pub(super) fn vacuum_headroom(database_path: &Path) -> Result<VacuumHeadroom, Error> {
    let estimated_required_bytes = database_path
        .metadata()
        .map_err(|source| io_error(database_path, source))?
        .len();
    let available_bytes =
        fs2::available_space(database_path).map_err(|source| io_error(database_path, source))?;
    Ok(VacuumHeadroom {
        estimated_required_bytes,
        available_bytes,
        vacuum_into_feasible: available_bytes >= estimated_required_bytes,
    })
}

pub(super) fn auto_vacuum(connection: &Connection) -> Result<AutoVacuumReport, Error> {
    let value = connection
        .pragma_query_value(None, "auto_vacuum", |row| row.get::<_, u32>(0))
        .map_err(|source| sqlite_error("reading PRAGMA auto_vacuum", source))?;
    let (mode, incremental_applicable) = match value {
        0 => ("none", false),
        1 => ("full", false),
        2 => ("incremental", true),
        _ => ("unknown", false),
    };
    Ok(AutoVacuumReport {
        value,
        mode,
        incremental_applicable,
    })
}

pub(super) fn timestamp_sanity(connection: &Connection) -> Result<TimestampSanity, Error> {
    let maximum = connection
        .query_row("SELECT MAX(time_updated) FROM session", [], |row| {
            row.get(0)
        })
        .map_err(|source| sqlite_error("probing maximum session time_updated", source))?;
    Ok(classify_timestamp(maximum))
}

fn classify_timestamp(maximum: Option<i64>) -> TimestampSanity {
    match maximum {
        Some(value) if (YEAR_2020_MILLISECONDS..=YEAR_2100_MILLISECONDS).contains(&value) => {
            TimestampSanity {
                max_time_updated: Some(value),
                ok: true,
                classification: "plausible-millisecond-epoch",
                detail: "maximum session time_updated is within the 2020-2100 millisecond epoch range"
                    .to_owned(),
            }
        }
        Some(value) if (YEAR_2020_SECONDS..=YEAR_2100_SECONDS).contains(&value) => TimestampSanity {
            max_time_updated: Some(value),
            ok: false,
            classification: "likely-second-epoch",
            detail: "maximum session time_updated looks like epoch seconds; cleanup predicates require milliseconds"
                .to_owned(),
        },
        Some(value) => TimestampSanity {
            max_time_updated: Some(value),
            ok: false,
            classification: "implausible-epoch",
            detail: "maximum session time_updated is outside the plausible 2020-2100 millisecond epoch range"
                .to_owned(),
        },
        None => TimestampSanity {
            max_time_updated: None,
            ok: true,
            classification: "empty-session-table",
            detail: "session table is empty; no timestamp value is available to classify".to_owned(),
        },
    }
}

pub(super) fn holders(database_path: &Path) -> HolderReport {
    let inspector = platform_inspector();
    let (inspection, _decision) = inspect_and_decide(
        inspector.as_ref(),
        database_path,
        CommandMode::Doctor,
        false,
    );
    holder_report(inspection)
}

fn holder_report(inspection: Inspection) -> HolderReport {
    let (completeness, completeness_discriminant) = match inspection.completeness {
        Completeness::CompleteForVisibleProcesses => ("complete", "CompleteForVisibleProcesses"),
        Completeness::PartialDueToPermissions => {
            ("partial-due-to-permissions", "PartialDueToPermissions")
        }
        Completeness::Unsupported => ("unsupported", "Unsupported"),
    };
    let (verdict, reason, processes) = match inspection.verdict {
        Verdict::Held(holders) => (
            "held",
            None,
            holders
                .into_iter()
                .map(|holder| HolderProcess {
                    pid: holder.pid,
                    name: holder.process_name,
                    observed_via: observation_method(),
                    matched_paths: holder.matched_paths,
                })
                .collect(),
        ),
        Verdict::NotHeld => ("not-held-at-scan-time", None, Vec::new()),
        Verdict::CannotDetermine(reason) => ("cannot-determine", Some(reason), Vec::new()),
    };
    HolderReport {
        verdict,
        reason,
        completeness,
        completeness_discriminant,
        processes,
    }
}

#[cfg(target_os = "linux")]
fn platform_inspector() -> Box<dyn HolderInspector> {
    Box::new(crate::safety::holders::linux::LinuxHolderInspector::default())
}

#[cfg(target_os = "macos")]
fn platform_inspector() -> Box<dyn HolderInspector> {
    Box::new(crate::safety::holders::macos::MacosHolderInspector)
}

#[cfg(windows)]
fn platform_inspector() -> Box<dyn HolderInspector> {
    Box::new(crate::safety::holders::windows::WindowsHolderInspector)
}

#[cfg(target_os = "linux")]
const fn observation_method() -> &'static str {
    "procfs file-descriptor target"
}

#[cfg(target_os = "macos")]
const fn observation_method() -> &'static str {
    "libproc vnode path lookup"
}

#[cfg(windows)]
const fn observation_method() -> &'static str {
    "Windows Restart Manager resource scan"
}

fn string_rows(connection: &Connection, sql: &str, context: &str) -> Result<Vec<String>, Error> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|source| sqlite_error(context, source))?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(|source| sqlite_error(context, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error(context, source))
}

fn integrity_error(check: &str, error: &Error) -> Error {
    Error::IntegrityCheckFailed {
        check: check.to_owned(),
        message: error.to_string(),
    }
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        context: context.to_owned(),
        source,
    }
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
#[path = "checks/tests.rs"]
mod tests;
