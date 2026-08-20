use std::io::Write;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::model::{DoctorReport, ForeignKeyFinding, HolderProcess};
use crate::analyze::orphans::{OrphanClass, OrphanReport};
use crate::error::Error;

pub(super) fn write(report: &DoctorReport, output: &mut dyn Write) -> Result<(), Error> {
    let value = json!({
        "schema_version": super::SCHEMA_VERSION,
        "database": report.database_path,
        "schema": {
            "tier_one": schema_tier(&report.schema.tier_one_missing, "compatible", "incompatible"),
            "tier_two": schema_tier(&report.schema.tier_two_warnings, "clear", "warning"),
            "tier_three": schema_tier(&report.schema.tier_three_findings, "clear", "finding"),
        },
        "integrity_check": {
            "ok": report.integrity_check.ok,
            "findings": report.integrity_check.findings,
        },
        "foreign_key_check": {
            "ok": report.foreign_key_check.ok,
            "findings": report.foreign_key_check.findings.iter().map(foreign_key).collect::<Vec<_>>(),
        },
        "orphans": orphans(&report.orphans),
        "holders": {
            "verdict": report.holders.verdict,
            "reason": report.holders.reason,
            "completeness": report.holders.completeness,
            "completeness_discriminant": report.holders.completeness_discriminant,
            "processes": report.holders.processes.iter().map(holder).collect::<Vec<_>>(),
            "snapshot_warning": "Holder detection is a point-in-time scan; another process may connect after it completes.",
        },
        "vacuum_headroom": {
            "estimated_required_bytes": report.vacuum_headroom.estimated_required_bytes,
            "available_bytes": report.vacuum_headroom.available_bytes,
            "vacuum_into_feasible": report.vacuum_headroom.vacuum_into_feasible,
            "estimate_basis": "Standalone upper-bound estimate using current database file size; a later shared reclaim module may formalize this calculation.",
        },
        "auto_vacuum": {
            "value": report.auto_vacuum.value,
            "mode": report.auto_vacuum.mode,
            "incremental_applicable": report.auto_vacuum.incremental_applicable,
        },
        "timestamp_sanity": {
            "max_time_updated": report.timestamp_sanity.max_time_updated,
            "ok": report.timestamp_sanity.ok,
            "classification": report.timestamp_sanity.classification,
            "detail": report.timestamp_sanity.detail,
        },
    });
    serde_json::to_writer_pretty(&mut *output, &value).map_err(json_error)?;
    writeln!(output).map_err(output_error)
}

fn schema_tier(findings: &[String], clear: &'static str, populated: &'static str) -> Value {
    json!({
        "status": if findings.is_empty() { clear } else { populated },
        "findings": findings,
    })
}

fn foreign_key(finding: &ForeignKeyFinding) -> Value {
    json!({
        "table": finding.table,
        "row_id": finding.row_id,
        "parent_table": finding.parent_table,
        "foreign_key_index": finding.foreign_key_index,
    })
}

fn orphans(report: &OrphanReport) -> Value {
    json!({
        "orphan_events": orphan(report.orphan_events),
        "dangling_parent_sessions": orphan(report.dangling_parent_sessions),
        "foreign_key_dangling_rows": orphan(report.foreign_key_dangling_rows),
        "orphan_storage_files": orphan(report.orphan_storage_files),
        "orphan_snapshot_directories": orphan(report.orphan_snapshot_directories),
    })
}

fn orphan(class: OrphanClass) -> Value {
    json!({ "count": class.count, "bytes": class.bytes })
}

fn holder(process: &HolderProcess) -> Value {
    json!({
        "pid": process.pid,
        "name": process.name,
        "observed_via": process.observed_via,
        "matched_paths": process
            .matched_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
    })
}

fn json_error(source: serde_json::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stdout>"),
        source: std::io::Error::other(source),
    }
}

fn output_error(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stdout>"),
        source,
    }
}
