use std::io::{self, Write};

use serde_json::json;

use crate::report::impact::{self, ImpactSummary};

#[derive(Debug)]
pub(super) struct CleanReport {
    pub(super) impact: ImpactSummary,
    pub(super) applied: bool,
    pub(super) deleted_sessions: u64,
    pub(super) deleted_rows: u64,
    pub(super) pruned_projects: u64,
    pub(super) storage_files_removed: u64,
    pub(super) snapshot_directories_removed: u64,
    pub(super) bytes_reclaimed: u64,
    pub(super) partial_failures: Vec<String>,
}

pub(super) fn write_dry_run(
    summary: &ImpactSummary,
    json_output: bool,
    output: &mut dyn Write,
) -> io::Result<()> {
    if json_output {
        write_json("dry-run", summary, 0, 0, 0, 0, 0, &[], output)
    } else {
        impact::write_human(summary, output)
    }
}

pub(super) fn write_final(
    report: &CleanReport,
    json_output: bool,
    output: &mut dyn Write,
) -> io::Result<()> {
    if json_output {
        return write_json(
            if report.applied { "applied" } else { "dry-run" },
            &report.impact,
            report.deleted_sessions,
            report.deleted_rows,
            report.pruned_projects,
            report.storage_files_removed,
            report.snapshot_directories_removed,
            &report.partial_failures,
            output,
        );
    }
    writeln!(output, "Cleanup complete")?;
    writeln!(output, "  Sessions deleted: {}", report.deleted_sessions)?;
    writeln!(output, "  Rows deleted: {}", report.deleted_rows)?;
    writeln!(output, "  Projects pruned: {}", report.pruned_projects)?;
    writeln!(
        output,
        "  Storage files removed: {}",
        report.storage_files_removed
    )?;
    writeln!(
        output,
        "  Snapshot directories removed: {}",
        report.snapshot_directories_removed
    )?;
    writeln!(
        output,
        "  Database bytes reclaimed: {}",
        report.bytes_reclaimed
    )?;
    for failure in &report.partial_failures {
        writeln!(output, "  Warning: {failure}")?;
    }
    Ok(())
}

pub(super) fn write_interrupted(
    deleted_sessions: u64,
    json_output: bool,
    output: &mut dyn Write,
) -> io::Result<()> {
    if json_output {
        serde_json::to_writer(
            &mut *output,
            &json!({
                "status": "interrupted",
                "deleted_sessions": deleted_sessions,
                "message": format!("deleted {deleted_sessions}, stopped safely"),
            }),
        )
        .map_err(io::Error::other)?;
        writeln!(output)
    } else {
        writeln!(output, "deleted {deleted_sessions}, stopped safely")
    }
}

#[allow(clippy::too_many_arguments)]
fn write_json(
    status: &str,
    summary: &ImpactSummary,
    deleted_sessions: u64,
    deleted_rows: u64,
    pruned_projects: u64,
    storage_files_removed: u64,
    snapshot_directories_removed: u64,
    partial_failures: &[String],
    output: &mut dyn Write,
) -> io::Result<()> {
    serde_json::to_writer(
        &mut *output,
        &json!({
            "status": status,
            "impact": {
                "root_sessions": summary.root_session_count,
                "total_sessions": summary.total_session_count,
                "table_rows": summary.table_rows,
                "orphan_rows": summary.orphan_row_count,
                "storage_files": summary.storage_file_count,
                "snapshot_directories": summary.snapshot_directory_count,
                "projects": summary.project_prune_count,
                "total_bytes": summary.total_bytes,
                "estimated_post_vacuum_bytes": summary.estimated_post_vacuum_bytes,
            },
            "deleted_sessions": deleted_sessions,
            "deleted_rows": deleted_rows,
            "pruned_projects": pruned_projects,
            "storage_files_removed": storage_files_removed,
            "snapshot_directories_removed": snapshot_directories_removed,
            "partial_failures": partial_failures,
        }),
    )
    .map_err(io::Error::other)?;
    writeln!(output)
}
