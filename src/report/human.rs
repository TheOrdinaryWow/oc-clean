use std::io::Write;

use super::format::{self, Align, Grid, Style};
use super::{AnalysisReport, ReportMode};
use crate::analyze::attribution::{ProjectAttribution, SessionAttribution};
use crate::analyze::distribution::{AgeRange, DirectoryGroup, DirectorySummary};
use crate::error::Error;

/// Width budget for a session title before the column arrangement takes over.
///
/// A title is arbitrary user text, so it is bounded before reaching the table; without a cap,
/// one long title would starve every other column of width.
const TITLE_WIDTH: usize = 48;

pub(super) fn write(
    report: &AnalysisReport,
    output: &mut dyn Write,
    style: Style,
) -> Result<(), Error> {
    file_space(report, output, style)?;
    row_counts(report, output, style)?;

    if report.mode == ReportMode::Quick {
        return Ok(());
    }
    table_space(report, output, style)?;
    project_attribution(report, output, style)?;
    largest_sessions(report, output, style)?;
    orphans(report, output, style)?;
    age_distribution(report, output, style)?;
    external_directories(report, output, style)
}

fn file_space(report: &AnalysisReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Database File Space", style)?;
    let space = &report.file_space;
    format::field(output, "Page count", space.page_count)?;
    format::field(output, "Page size", format::bytes(space.page_size.into()))?;
    format::field(output, "Total", format::bytes(space.total_bytes))?;
    format::field(output, "Live", format::bytes(space.live_bytes))?;
    format::field(
        output,
        "Freelist",
        format!(
            "{} ({:.2}%)",
            format::bytes(space.freelist_bytes),
            space.freelist_percent
        ),
    )?;
    format::field(output, "WAL", format::optional_bytes(space.wal_bytes))?;
    format::field(output, "SHM", format::optional_bytes(space.shm_bytes))
}

fn row_counts(report: &AnalysisReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Row Counts", style)?;
    let mut grid = Grid::new(style, &[("Table", Align::Left), ("Rows", Align::Right)]);
    for (table, rows) in &report.row_counts {
        grid.row(vec![table.clone(), rows.to_string()]);
    }
    grid.write(output)
}

fn table_space(report: &AnalysisReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Table and Index Space", style)?;
    let table_space = report
        .table_space
        .as_ref()
        .expect("full report has table space");
    format::field(output, "Accounting", table_space.label)?;
    let total = table_space
        .entries
        .iter()
        .map(|entry| entry.bytes)
        .sum::<u64>();
    let mut grid = Grid::new(
        style,
        &[
            ("Object", Align::Left),
            ("Size", Align::Right),
            ("Share", Align::Right),
        ],
    );
    for entry in &table_space.entries {
        grid.row(vec![
            entry.name.clone(),
            format::bytes(entry.bytes),
            format::percent_of(entry.bytes, total),
        ]);
    }
    grid.write(output)
}

fn project_attribution(
    report: &AnalysisReport,
    output: &mut dyn Write,
    style: Style,
) -> Result<(), Error> {
    format::heading(output, "Project Attribution", style)?;
    let projects: &Vec<ProjectAttribution> = report
        .project_attribution
        .as_ref()
        .expect("full report has projects");
    format::field(output, "Projects", projects.len())?;
    let total = projects.iter().map(|project| project.bytes).sum::<u64>();
    let mut grid = Grid::new(
        style,
        &[
            ("Project", Align::Left),
            ("Size", Align::Right),
            ("Share", Align::Right),
        ],
    );
    for project in projects {
        grid.row(vec![
            project.project_id.clone(),
            format::bytes(project.bytes),
            format::percent_of(project.bytes, total),
        ]);
    }
    grid.write(output)
}

fn largest_sessions(
    report: &AnalysisReport,
    output: &mut dyn Write,
    style: Style,
) -> Result<(), Error> {
    format::heading(output, "Largest Sessions", style)?;
    let sessions: &Vec<SessionAttribution> = report
        .largest_sessions
        .as_ref()
        .expect("full report has sessions");
    let mut grid = Grid::new(
        style,
        &[
            ("Session", Align::Left),
            ("Title", Align::Left),
            ("Msgs", Align::Right),
            ("Last active", Align::Left),
            ("Subtree", Align::Right),
            ("Self", Align::Right),
            ("Self %", Align::Right),
        ],
    );
    for session in sessions {
        let (title, messages, last_active) = session.details.as_ref().map_or_else(
            || {
                (
                    "(unavailable)".to_owned(),
                    String::from("-"),
                    String::from("-"),
                )
            },
            |details| {
                (
                    format::sanitize(&details.title, TITLE_WIDTH),
                    details.message_count.to_string(),
                    format::timestamp_ms(details.time_updated_ms),
                )
            },
        );
        grid.row(vec![
            style.dim(&format::short_id(&session.session_id)),
            title,
            messages,
            style.dim(&last_active),
            format::bytes(session.subtree_bytes),
            format::bytes(session.self_bytes),
            format::percent_of(session.self_bytes, session.subtree_bytes),
        ]);
    }
    grid.write(output)
}

fn orphans(report: &AnalysisReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Orphan Census", style)?;
    let orphans = report.orphans.as_ref().expect("full report has orphans");
    let classes = [
        ("Orphan events", orphans.orphan_events),
        ("Dangling parents", orphans.dangling_parent_sessions),
        ("Foreign-key rows", orphans.foreign_key_dangling_rows),
        ("Storage files", orphans.orphan_storage_files),
        ("Snapshot directories", orphans.orphan_snapshot_directories),
    ];
    let total = classes.iter().map(|(_, class)| class.bytes).sum::<u64>();
    let mut grid = Grid::new(
        style,
        &[
            ("Class", Align::Left),
            ("Count", Align::Right),
            ("Size", Align::Right),
            ("Share", Align::Right),
        ],
    );
    for (label, class) in classes {
        grid.row(vec![
            label.to_owned(),
            class.count.to_string(),
            format::bytes(class.bytes),
            format::percent_of(class.bytes, total),
        ]);
    }
    grid.write(output)
}

fn age_distribution(
    report: &AnalysisReport,
    output: &mut dyn Write,
    style: Style,
) -> Result<(), Error> {
    format::heading(output, "Age Distribution", style)?;
    let buckets = report
        .age_distribution
        .as_ref()
        .expect("full report has age buckets");
    let total = buckets.iter().map(|bucket| bucket.bytes).sum::<u64>();
    let mut grid = Grid::new(
        style,
        &[
            ("Age", Align::Left),
            ("Sessions", Align::Right),
            ("Size", Align::Right),
            ("Share", Align::Right),
        ],
    );
    for bucket in buckets {
        grid.row(vec![
            age_label(bucket.range).to_owned(),
            bucket.sessions.to_string(),
            format::bytes(bucket.bytes),
            format::percent_of(bucket.bytes, total),
        ]);
    }
    grid.write(output)
}

fn external_directories(
    report: &AnalysisReport,
    output: &mut dyn Write,
    style: Style,
) -> Result<(), Error> {
    format::heading(output, "External Directories", style)?;
    let external = report
        .external_directories
        .as_ref()
        .expect("full report has directories");
    let total = external.storage.total.bytes
        + external.snapshot.total.bytes
        + external.tool_output.bytes
        + external.log.bytes;
    let mut grid = Grid::new(
        style,
        &[
            ("Directory", Align::Left),
            ("Files", Align::Right),
            ("Size", Align::Right),
            ("Share", Align::Right),
        ],
    );
    directory_group(&mut grid, "storage", &external.storage, total);
    directory_group(&mut grid, "snapshot", &external.snapshot, total);
    directory_summary(&mut grid, "tool-output", external.tool_output, total);
    directory_summary(&mut grid, "log", external.log, total);
    grid.write(output)
}

fn directory_group(grid: &mut Grid, label: &str, group: &DirectoryGroup, overall_bytes: u64) {
    directory_summary(grid, label, group.total, overall_bytes);
    for entry in &group.entries {
        grid.row(vec![
            format!("  {}", entry.name),
            entry.files.to_string(),
            format::bytes(entry.bytes),
            format::percent_of(entry.bytes, group.total.bytes),
        ]);
    }
}

fn directory_summary(grid: &mut Grid, label: &str, summary: DirectorySummary, total: u64) {
    grid.row(vec![
        label.to_owned(),
        summary.files.to_string(),
        format::bytes(summary.bytes),
        format::percent_of(summary.bytes, total),
    ]);
}

const fn age_label(range: AgeRange) -> &'static str {
    match range {
        AgeRange::Days0To7 => "0-7D",
        AgeRange::Days7To30 => "7-30D",
        AgeRange::Days30To90 => "30-90D",
        AgeRange::Days90To180 => "90-180D",
        AgeRange::Days180Plus => "180D+",
    }
}
