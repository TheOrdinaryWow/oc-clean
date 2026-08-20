use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;

use super::model::DoctorReport;
use crate::error::Error;

pub(super) fn write(report: &DoctorReport, output: &mut dyn Write) -> Result<(), Error> {
    heading(output, "Schema Compatibility")?;
    schema_tier(output, "Tier 1 required", &report.schema.tier_one_missing)?;
    schema_tier(
        output,
        "Tier 2 extensions",
        &report.schema.tier_two_warnings,
    )?;
    schema_tier(
        output,
        "Tier 3 semantics",
        &report.schema.tier_three_findings,
    )?;

    heading(output, "Integrity Check")?;
    value(output, "Verdict", pass_fail(report.integrity_check.ok))?;
    findings(output, &report.integrity_check.findings)?;

    heading(output, "Foreign Key Check")?;
    value(output, "Verdict", pass_fail(report.foreign_key_check.ok))?;
    for finding in &report.foreign_key_check.findings {
        writeln!(
            output,
            "  {} row {:?} -> {} (FK #{})",
            finding.table, finding.row_id, finding.parent_table, finding.foreign_key_index
        )
        .map_err(output_error)?;
    }

    orphan_census(report, output)?;
    holders(report, output)?;
    headroom(report, output)?;
    auto_vacuum(report, output)?;
    timestamp(report, output)
}

fn orphan_census(report: &DoctorReport, output: &mut dyn Write) -> Result<(), Error> {
    heading(output, "Orphan Census")?;
    let orphans = &report.orphans;
    orphan(output, "Orphan events", orphans.orphan_events)?;
    orphan(
        output,
        "Dangling parent sessions",
        orphans.dangling_parent_sessions,
    )?;
    orphan(
        output,
        "Foreign key dangling rows",
        orphans.foreign_key_dangling_rows,
    )?;
    orphan(output, "Orphan storage files", orphans.orphan_storage_files)?;
    orphan(
        output,
        "Orphan snapshot directories",
        orphans.orphan_snapshot_directories,
    )
}

fn holders(report: &DoctorReport, output: &mut dyn Write) -> Result<(), Error> {
    heading(output, "Database Holders")?;
    value(output, "Verdict", report.holders.verdict)?;
    value(output, "Scan completeness", report.holders.completeness)?;
    value(
        output,
        "Completeness discriminant",
        report.holders.completeness_discriminant,
    )?;
    if let Some(reason) = &report.holders.reason {
        value(output, "Reason", reason)?;
    }
    for process in &report.holders.processes {
        let name = process.name.as_deref().unwrap_or("unknown process");
        writeln!(
            output,
            "  pid {} ({name}), observed via {}",
            process.pid, process.observed_via
        )
        .map_err(output_error)?;
        for path in &process.matched_paths {
            writeln!(output, "    {}", path.display()).map_err(output_error)?;
        }
    }
    writeln!(
        output,
        "  Holder detection is a point-in-time scan; another process may connect after it completes."
    )
    .map_err(output_error)
}

fn headroom(report: &DoctorReport, output: &mut dyn Write) -> Result<(), Error> {
    heading(output, "VACUUM Headroom")?;
    value(
        output,
        "Estimated required",
        format!("{} bytes", report.vacuum_headroom.estimated_required_bytes),
    )?;
    value(
        output,
        "Available",
        format!("{} bytes", report.vacuum_headroom.available_bytes),
    )?;
    value(
        output,
        "VACUUM INTO feasible",
        yes_no(report.vacuum_headroom.vacuum_into_feasible),
    )?;
    writeln!(
        output,
        "  Standalone upper-bound estimate: required space equals the current database file size."
    )
    .map_err(output_error)
}

fn auto_vacuum(report: &DoctorReport, output: &mut dyn Write) -> Result<(), Error> {
    heading(output, "Auto Vacuum")?;
    value(output, "PRAGMA value", report.auto_vacuum.value)?;
    value(output, "Mode", report.auto_vacuum.mode)?;
    value(
        output,
        "Incremental applicable",
        yes_no(report.auto_vacuum.incremental_applicable),
    )
}

fn timestamp(report: &DoctorReport, output: &mut dyn Write) -> Result<(), Error> {
    heading(output, "Timestamp Sanity")?;
    value(output, "Verdict", pass_fail(report.timestamp_sanity.ok))?;
    value(
        output,
        "Maximum time_updated",
        report
            .timestamp_sanity
            .max_time_updated
            .map_or_else(|| "none".to_owned(), |value| value.to_string()),
    )?;
    value(
        output,
        "Classification",
        report.timestamp_sanity.classification,
    )?;
    value(output, "Detail", &report.timestamp_sanity.detail)
}

fn schema_tier(output: &mut dyn Write, label: &str, tier_findings: &[String]) -> Result<(), Error> {
    value(
        output,
        label,
        if tier_findings.is_empty() {
            "clear"
        } else {
            "findings"
        },
    )?;
    findings(output, tier_findings)
}

fn findings(output: &mut dyn Write, findings: &[String]) -> Result<(), Error> {
    for finding in findings {
        writeln!(output, "  - {finding}").map_err(output_error)?;
    }
    Ok(())
}

fn orphan(
    output: &mut dyn Write,
    label: &str,
    class: crate::analyze::orphans::OrphanClass,
) -> Result<(), Error> {
    value(
        output,
        label,
        format!("{} ({} bytes)", class.count, class.bytes),
    )
}

fn heading(output: &mut dyn Write, title: &str) -> Result<(), Error> {
    writeln!(output, "\n{title}").map_err(output_error)
}

fn value(output: &mut dyn Write, label: &str, value: impl Display) -> Result<(), Error> {
    writeln!(output, "  {label:<28} {value}").map_err(output_error)
}

const fn pass_fail(ok: bool) -> &'static str {
    if ok { "pass" } else { "fail" }
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn output_error(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stdout>"),
        source,
    }
}
