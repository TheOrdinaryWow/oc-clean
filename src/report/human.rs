use std::io::Write;
use std::path::PathBuf;

use super::{AnalysisReport, ReportMode};
use crate::analyze::distribution::{AgeRange, DirectoryGroup, DirectorySummary};
use crate::error::Error;

pub(super) fn write(
    report: &AnalysisReport,
    output: &mut dyn Write,
    color: bool,
) -> Result<(), Error> {
    heading(output, "Database File Space", color)?;
    value(output, "Page count", report.file_space.page_count)?;
    value(
        output,
        "Page size",
        bytes(report.file_space.page_size.into()),
    )?;
    value(output, "Total", bytes(report.file_space.total_bytes))?;
    value(output, "Live", bytes(report.file_space.live_bytes))?;
    value(
        output,
        "Freelist",
        format!(
            "{} ({:.2}%)",
            bytes(report.file_space.freelist_bytes),
            report.file_space.freelist_percent
        ),
    )?;
    value(output, "WAL", optional_bytes(report.file_space.wal_bytes))?;
    value(output, "SHM", optional_bytes(report.file_space.shm_bytes))?;

    heading(output, "Row Counts", color)?;
    for (table, rows) in &report.row_counts {
        value(output, table, rows)?;
    }

    if report.mode == ReportMode::Quick {
        return Ok(());
    }
    table_space(report, output, color)?;
    project_attribution(report, output, color)?;
    largest_sessions(report, output, color)?;
    orphans(report, output, color)?;
    age_distribution(report, output, color)?;
    external_directories(report, output, color)
}

pub(super) const fn color_enabled(stdout_is_terminal: bool, no_color_is_set: bool) -> bool {
    stdout_is_terminal && !no_color_is_set
}

fn table_space(report: &AnalysisReport, output: &mut dyn Write, color: bool) -> Result<(), Error> {
    heading(output, "Table and Index Space", color)?;
    let table_space = report
        .table_space
        .as_ref()
        .expect("full report has table space");
    value(output, "Accounting", table_space.label)?;
    let total = table_space
        .entries
        .iter()
        .map(|entry| entry.bytes)
        .sum::<u64>();
    for entry in &table_space.entries {
        row(
            output,
            &entry.name,
            entry.bytes,
            percent(entry.bytes, total),
        )?;
    }
    Ok(())
}

fn project_attribution(
    report: &AnalysisReport,
    output: &mut dyn Write,
    color: bool,
) -> Result<(), Error> {
    heading(output, "Project Attribution", color)?;
    let projects = report
        .project_attribution
        .as_ref()
        .expect("full report has projects");
    value(output, "Projects", projects.len())?;
    let total = projects.iter().map(|project| project.bytes).sum::<u64>();
    for project in projects {
        row(
            output,
            &project.project_id,
            project.bytes,
            percent(project.bytes, total),
        )?;
    }
    Ok(())
}

fn largest_sessions(
    report: &AnalysisReport,
    output: &mut dyn Write,
    color: bool,
) -> Result<(), Error> {
    heading(output, "Largest Sessions", color)?;
    for session in report
        .largest_sessions
        .as_ref()
        .expect("full report has sessions")
    {
        writeln!(
            output,
            "  {:<25}{:>14} subtree  {:>14} self  {:>7.2}%  {}",
            session.session_id,
            bytes(session.subtree_bytes),
            bytes(session.self_bytes),
            percent(session.self_bytes, session.subtree_bytes),
            session.project_id
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn orphans(report: &AnalysisReport, output: &mut dyn Write, color: bool) -> Result<(), Error> {
    heading(output, "Orphan Census", color)?;
    let report = report.orphans.as_ref().expect("full report has orphans");
    let total = [
        report.orphan_events,
        report.dangling_parent_sessions,
        report.foreign_key_dangling_rows,
        report.orphan_storage_files,
        report.orphan_snapshot_directories,
    ]
    .iter()
    .map(|orphan| orphan.bytes)
    .sum();
    orphan_row(output, "Orphan events", report.orphan_events, total)?;
    orphan_row(
        output,
        "Dangling parents",
        report.dangling_parent_sessions,
        total,
    )?;
    orphan_row(
        output,
        "Foreign-key rows",
        report.foreign_key_dangling_rows,
        total,
    )?;
    orphan_row(output, "Storage files", report.orphan_storage_files, total)?;
    orphan_row(
        output,
        "Snapshot directories",
        report.orphan_snapshot_directories,
        total,
    )
}

fn age_distribution(
    report: &AnalysisReport,
    output: &mut dyn Write,
    color: bool,
) -> Result<(), Error> {
    heading(output, "Age Distribution", color)?;
    let buckets = report
        .age_distribution
        .as_ref()
        .expect("full report has age buckets");
    let total = buckets.iter().map(|bucket| bucket.bytes).sum::<u64>();
    for bucket in buckets {
        writeln!(
            output,
            "  {:<25}{:>8} sessions  {:>14}  {:>7.2}%",
            age_label(bucket.range),
            bucket.sessions,
            bytes(bucket.bytes),
            percent(bucket.bytes, total)
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn external_directories(
    report: &AnalysisReport,
    output: &mut dyn Write,
    color: bool,
) -> Result<(), Error> {
    heading(output, "External Directories", color)?;
    let external = report
        .external_directories
        .as_ref()
        .expect("full report has directories");
    let total = external.storage.total.bytes
        + external.snapshot.total.bytes
        + external.tool_output.bytes
        + external.log.bytes;
    directory_group(output, "storage", &external.storage, total)?;
    directory_group(output, "snapshot", &external.snapshot, total)?;
    directory_summary(output, "tool-output", external.tool_output, total)?;
    directory_summary(output, "log", external.log, total)
}

fn heading(output: &mut dyn Write, title: &str, color: bool) -> Result<(), Error> {
    if color {
        writeln!(output, "\n\x1b[1;36m{title}\x1b[0m").map_err(output_error)
    } else {
        writeln!(output, "\n{title}").map_err(output_error)
    }
}

fn value(output: &mut dyn Write, label: &str, value: impl std::fmt::Display) -> Result<(), Error> {
    writeln!(output, "  {label:<25}{value}").map_err(output_error)
}

fn row(output: &mut dyn Write, label: &str, size: u64, percent: f64) -> Result<(), Error> {
    writeln!(output, "  {label:<25}{:>14}  {percent:>7.2}%", bytes(size)).map_err(output_error)
}

fn orphan_row(
    output: &mut dyn Write,
    label: &str,
    orphan: crate::analyze::orphans::OrphanClass,
    total: u64,
) -> Result<(), Error> {
    writeln!(
        output,
        "  {label:<25}{:<8}{:>14}  {:>7.2}%",
        orphan.count,
        bytes(orphan.bytes),
        percent(orphan.bytes, total)
    )
    .map_err(output_error)
}

fn directory_group(
    output: &mut dyn Write,
    label: &str,
    group: &DirectoryGroup,
    overall_bytes: u64,
) -> Result<(), Error> {
    directory_summary(output, label, group.total, overall_bytes)?;
    for entry in &group.entries {
        writeln!(
            output,
            "    {:<23}{:<8}{:>14}  {:>7.2}%",
            entry.name,
            entry.files,
            bytes(entry.bytes),
            percent(entry.bytes, group.total.bytes)
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn directory_summary(
    output: &mut dyn Write,
    label: &str,
    summary: DirectorySummary,
    total: u64,
) -> Result<(), Error> {
    writeln!(
        output,
        "  {label:<25}{:<8}{:>14}  {:>7.2}%",
        summary.files,
        bytes(summary.bytes),
        percent(summary.bytes, total)
    )
    .map_err(output_error)
}

fn age_label(range: AgeRange) -> &'static str {
    match range {
        AgeRange::Days0To7 => "0-7D",
        AgeRange::Days7To30 => "7-30D",
        AgeRange::Days30To90 => "30-90D",
        AgeRange::Days90To180 => "90-180D",
        AgeRange::Days180Plus => "180D+",
    }
}

#[allow(clippy::cast_precision_loss)]
fn bytes(value: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1024.0 && unit < UNITS.len() - 1 {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{scaled:.2} {} ({value} bytes)", UNITS[unit])
    }
}

fn optional_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "not present".to_owned(), bytes)
}

#[allow(clippy::cast_precision_loss)]
fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 * 100.0 / total as f64
    }
}

fn output_error(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stdout>"),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::color_enabled;

    #[test]
    fn color_requires_a_terminal_without_no_color() {
        assert!(color_enabled(true, false));
        assert!(!color_enabled(false, false));
        assert!(!color_enabled(true, true));
    }
}
