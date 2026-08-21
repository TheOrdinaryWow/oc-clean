use std::io::Write;

use super::model::DoctorReport;
use crate::error::Error;
use crate::report::format::{self, Align, Grid, Style};

pub(super) fn write(
    report: &DoctorReport,
    output: &mut dyn Write,
    style: Style,
) -> Result<(), Error> {
    schema(report, output, style)?;
    integrity(report, output, style)?;
    foreign_keys(report, output, style)?;
    orphan_census(report, output, style)?;
    holders(report, output, style)?;
    headroom(report, output, style)?;
    auto_vacuum(report, output, style)?;
    timestamp(report, output, style)
}

fn schema(report: &DoctorReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Schema Compatibility", style)?;
    schema_tier(
        output,
        "Tier 1 required",
        &report.schema.tier_one_missing,
        style,
        Severity::Blocking,
    )?;
    schema_tier(
        output,
        "Tier 2 extensions",
        &report.schema.tier_two_warnings,
        style,
        Severity::Advisory,
    )?;
    schema_tier(
        output,
        "Tier 3 semantics",
        &report.schema.tier_three_findings,
        style,
        Severity::Blocking,
    )
}

fn integrity(report: &DoctorReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Integrity Check", style)?;
    format::field(output, "Verdict", verdict(report.integrity_check.ok, style))?;
    if report.integrity_check.ok {
        // A passing `PRAGMA integrity_check` reports the single finding "ok", which the
        // verdict above already states. The JSON report still carries it verbatim.
        return Ok(());
    }
    findings(output, &report.integrity_check.findings)
}

fn foreign_keys(report: &DoctorReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Foreign Key Check", style)?;
    format::field(
        output,
        "Verdict",
        verdict(report.foreign_key_check.ok, style),
    )?;
    if report.foreign_key_check.findings.is_empty() {
        return Ok(());
    }
    let mut grid = Grid::new(
        style,
        &[
            ("Table", Align::Left),
            ("Row", Align::Right),
            ("Parent", Align::Left),
            ("FK", Align::Right),
        ],
    );
    for finding in &report.foreign_key_check.findings {
        grid.row(vec![
            finding.table.clone(),
            format!("{:?}", finding.row_id),
            finding.parent_table.clone(),
            finding.foreign_key_index.to_string(),
        ]);
    }
    grid.write(output)
}

fn orphan_census(report: &DoctorReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Orphan Census", style)?;
    let orphans = &report.orphans;
    let mut grid = Grid::new(
        style,
        &[
            ("Class", Align::Left),
            ("Count", Align::Right),
            ("Size", Align::Right),
        ],
    );
    for (label, class) in [
        ("Orphan events", orphans.orphan_events),
        ("Dangling parent sessions", orphans.dangling_parent_sessions),
        (
            "Foreign key dangling rows",
            orphans.foreign_key_dangling_rows,
        ),
        ("Orphan storage files", orphans.orphan_storage_files),
        (
            "Orphan snapshot directories",
            orphans.orphan_snapshot_directories,
        ),
    ] {
        grid.row(vec![
            label.to_owned(),
            class.count.to_string(),
            format::bytes(class.bytes),
        ]);
    }
    grid.write(output)
}

fn holders(report: &DoctorReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Database Holders", style)?;
    // An observed holder blocks destructive work, so it reads as a failure even though
    // `doctor` itself only reports it. An indeterminate scan is neither: it means the answer
    // is unknown, which is a caution rather than a clean result.
    let held = !report.holders.processes.is_empty();
    format::field(
        output,
        "Verdict",
        match report.holders.verdict {
            "held" => style.fail(report.holders.verdict),
            "not-held-at-scan-time" => style.pass(report.holders.verdict),
            other => style.warn(other),
        },
    )?;
    format::field(output, "Scan completeness", report.holders.completeness)?;
    format::field(
        output,
        "Completeness discriminant",
        report.holders.completeness_discriminant,
    )?;
    if let Some(reason) = &report.holders.reason {
        format::field(output, "Reason", reason)?;
    }
    if held {
        let mut grid = Grid::new(
            style,
            &[
                ("PID", Align::Right),
                ("Process", Align::Left),
                ("Observed via", Align::Left),
                ("Path", Align::Left),
            ],
        );
        for process in &report.holders.processes {
            let name = process.name.as_deref().unwrap_or("unknown process");
            let mut paths = process.matched_paths.iter();
            grid.row(vec![
                process.pid.to_string(),
                name.to_owned(),
                process.observed_via.to_owned(),
                paths
                    .next()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            ]);
            // Later paths continue the same process's row rather than repeating its identity.
            for path in paths {
                grid.row(vec![
                    String::new(),
                    String::new(),
                    String::new(),
                    path.display().to_string(),
                ]);
            }
        }
        grid.write(output)?;
    }
    format::note(
        output,
        "Holder detection is a point-in-time scan; another process may connect after it completes.",
        style,
    )
}

fn headroom(report: &DoctorReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "VACUUM Headroom", style)?;
    format::field(
        output,
        "Estimated required",
        format::bytes(report.vacuum_headroom.estimated_required_bytes),
    )?;
    format::field(
        output,
        "Available",
        format::bytes(report.vacuum_headroom.available_bytes),
    )?;
    format::field(
        output,
        "VACUUM INTO feasible",
        verdict_words(
            report.vacuum_headroom.vacuum_into_feasible,
            "yes",
            "no",
            style,
        ),
    )?;
    format::note(
        output,
        "Standalone upper-bound estimate: required space equals the current database file size.",
        style,
    )
}

fn auto_vacuum(report: &DoctorReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Auto Vacuum", style)?;
    format::field(output, "PRAGMA value", report.auto_vacuum.value)?;
    format::field(output, "Mode", report.auto_vacuum.mode)?;
    format::field(
        output,
        "Incremental applicable",
        // An inapplicable mode is a fact about the database, not a defect, so it is styled
        // as an advisory rather than a failure.
        if report.auto_vacuum.incremental_applicable {
            style.pass("yes")
        } else {
            style.warn("no")
        },
    )
}

fn timestamp(report: &DoctorReport, output: &mut dyn Write, style: Style) -> Result<(), Error> {
    format::heading(output, "Timestamp Sanity", style)?;
    format::field(
        output,
        "Verdict",
        verdict(report.timestamp_sanity.ok, style),
    )?;
    format::field(
        output,
        "Maximum time_updated",
        report
            .timestamp_sanity
            .max_time_updated
            .map_or_else(|| "none".to_owned(), format::timestamp_ms),
    )?;
    format::field(
        output,
        "Classification",
        report.timestamp_sanity.classification,
    )?;
    format::field(output, "Detail", &report.timestamp_sanity.detail)
}

/// How a non-empty schema tier affects command execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Severity {
    /// Findings stop compatibility-enforcing commands.
    Blocking,
    /// Findings are tolerated and reported for awareness.
    Advisory,
}

fn schema_tier(
    output: &mut dyn Write,
    label: &str,
    tier_findings: &[String],
    style: Style,
    severity: Severity,
) -> Result<(), Error> {
    let rendered = if tier_findings.is_empty() {
        style.pass("clear")
    } else if severity == Severity::Blocking {
        style.fail("findings")
    } else {
        style.warn("findings")
    };
    format::field(output, label, rendered)?;
    findings(output, tier_findings)
}

fn findings(output: &mut dyn Write, findings: &[String]) -> Result<(), Error> {
    for finding in findings {
        format::bullet(output, finding)?;
    }
    Ok(())
}

fn verdict(ok: bool, style: Style) -> String {
    verdict_words(ok, "pass", "fail", style)
}

fn verdict_words(ok: bool, affirmative: &str, negative: &str, style: Style) -> String {
    if ok {
        style.pass(affirmative)
    } else {
        style.fail(negative)
    }
}
