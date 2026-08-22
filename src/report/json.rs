use std::io::Write;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::{AnalysisReport, ReportMode, SCHEMA_VERSION};
use crate::analyze::attribution::SessionAttribution;
use crate::analyze::distribution::{AgeRange, DirectoryGroup, DirectorySummary};
use crate::error::Error;

pub(super) fn write(report: &AnalysisReport, output: &mut dyn Write) -> Result<(), Error> {
    let value = json!({
        "schema_version": SCHEMA_VERSION,
        "mode": match report.mode { ReportMode::Full => "full", ReportMode::Quick => "quick" },
        "file_space": file_space(report),
        "row_counts": report.row_counts,
        "table_space": report.table_space.as_ref().map(table_space),
        "project_attribution": report.project_attribution.as_ref().map(|projects| projects.iter().map(|project| json!({
            "project_id": project.project_id,
            "worktree": project.worktree,
            "bytes": project.bytes,
        })).collect::<Vec<_>>()),
        "largest_sessions": report.largest_sessions.as_ref().map(|sessions| sessions.iter().map(session_object).collect::<Vec<_>>()),
        "orphans": report.orphans.as_ref().map(|orphans| json!({
            "orphan_events": orphan(orphans.orphan_events),
            "dangling_parent_sessions": orphan(orphans.dangling_parent_sessions),
            "foreign_key_dangling_rows": orphan(orphans.foreign_key_dangling_rows),
            "orphan_storage_files": orphan(orphans.orphan_storage_files),
            "orphan_snapshot_directories": orphan(orphans.orphan_snapshot_directories),
        })),
        "age_distribution": report.age_distribution.as_ref().map(|buckets| buckets.iter().map(|bucket| json!({
            "range": age_range(bucket.range),
            "sessions": bucket.sessions,
            "bytes": bucket.bytes,
        })).collect::<Vec<_>>()),
        "external_directories": report.external_directories.as_ref().map(|external| json!({
            "storage": directory_group(&external.storage),
            "snapshot": directory_group(&external.snapshot),
            "tool_output": directory_summary(external.tool_output),
            "log": directory_summary(external.log),
        })),
    });
    serde_json::to_writer_pretty(&mut *output, &value).map_err(json_error)?;
    writeln!(output).map_err(output_error)
}

fn file_space(report: &AnalysisReport) -> Value {
    let file = &report.file_space;
    json!({
        "page_count": file.page_count,
        "freelist_count": file.freelist_count,
        "page_size": file.page_size,
        "total_bytes": file.total_bytes,
        "live_bytes": file.live_bytes,
        "freelist_bytes": file.freelist_bytes,
        "freelist_percent": file.freelist_percent,
        "wal_bytes": file.wal_bytes,
        "shm_bytes": file.shm_bytes,
    })
}

fn table_space(report: &crate::analyze::space::ObjectSpaceReport) -> Value {
    json!({
        "method": match report.method {
            crate::analyze::space::AccountingMethod::Dbstat => "dbstat",
            crate::analyze::space::AccountingMethod::OctetLengthEstimate => "octet_length_estimate",
        },
        "label": report.label,
        "objects": report.entries.iter().map(|entry| json!({
            "name": entry.name,
            "kind": match entry.kind {
                crate::analyze::space::ObjectKind::Table => "table",
                crate::analyze::space::ObjectKind::Index => "index",
                crate::analyze::space::ObjectKind::Schema => "schema",
            },
            "bytes": entry.bytes,
        })).collect::<Vec<_>>(),
    })
}

/// Serializes one attributed session, including its description when one was looked up.
///
/// `title`, `time_updated`, `message_count`, and `project_path` are additive fields within the
/// current `schema_version`: the first three are absent for a session whose row disappeared
/// between the size rollup and the description lookup, and `project_path` is additionally absent
/// when the named project row no longer exists. Consumers must treat all four as optional.
fn session_object(session: &SessionAttribution) -> serde_json::Value {
    let mut object = json!({
        "session_id": session.session_id,
        "project_id": session.project_id,
        "self_bytes": session.self_bytes,
        "subtree_bytes": session.subtree_bytes,
    });
    if let (Some(details), Some(map)) = (session.details.as_ref(), object.as_object_mut()) {
        map.insert("title".to_owned(), json!(details.title));
        map.insert("time_updated".to_owned(), json!(details.time_updated_ms));
        map.insert("message_count".to_owned(), json!(details.message_count));
        if let Some(project_path) = &details.project_path {
            map.insert("project_path".to_owned(), json!(project_path));
        }
    }
    object
}

fn orphan(value: crate::analyze::orphans::OrphanClass) -> Value {
    json!({ "count": value.count, "bytes": value.bytes })
}

fn directory_group(group: &DirectoryGroup) -> Value {
    json!({
        "total": directory_summary(group.total),
        "entries": group.entries.iter().map(|entry| json!({
            "name": entry.name,
            "files": entry.files,
            "bytes": entry.bytes,
        })).collect::<Vec<_>>(),
    })
}

fn directory_summary(summary: DirectorySummary) -> Value {
    json!({ "files": summary.files, "bytes": summary.bytes })
}

const fn age_range(range: AgeRange) -> &'static str {
    match range {
        AgeRange::Days0To7 => "0-7D",
        AgeRange::Days7To30 => "7-30D",
        AgeRange::Days30To90 => "30-90D",
        AgeRange::Days90To180 => "90-180D",
        AgeRange::Days180Plus => "180D+",
    }
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
