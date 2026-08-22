//! Stable report model and stdout renderers for analysis commands.

pub mod command;
pub mod failure;
pub mod format;
mod human;
pub mod impact;
mod json;
pub mod logging;
pub mod progress;

use std::collections::BTreeMap;
use std::io::Write;

use crate::analyze::attribution::{AttributionReport, ProjectAttribution, SessionAttribution};
use crate::analyze::distribution::{AgeBucket, DistributionReport, ExternalDirectoryOverview};
use crate::analyze::orphans::OrphanReport;
use crate::analyze::space::{FileSpace, ObjectSpaceReport, SpaceReport};
use crate::error::Error;

/// Version of the documented JSON report contract.
pub const SCHEMA_VERSION: u32 = 2;

/// How much of the analysis an invocation asked for.
///
/// The split is by audience, not by cost. A standard report answers "what is taking up space and
/// can I delete it", which is what an operator opens `analyze` for. The detailed layers answer
/// questions about the database as a database, and burying the first set under the second made
/// the common case harder to read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportMode {
    Standard,
    Detailed,
}

/// Normalized report consumed by both renderers.
///
/// JSON always includes `schema_version`, `mode`, `file_space`, and all seven layer keys.
/// Detailed reports populate every layer. Standard reports set the four detail layers to `null`,
/// because the work behind them is skipped rather than merely hidden.
#[derive(Clone, Debug)]
pub struct AnalysisReport {
    mode: ReportMode,
    file_space: FileSpace,
    row_counts: Option<BTreeMap<String, u64>>,
    table_space: Option<ObjectSpaceReport>,
    project_attribution: Option<Vec<ProjectAttribution>>,
    largest_sessions: Option<Vec<SessionAttribution>>,
    orphans: Option<OrphanReport>,
    age_distribution: Option<Vec<AgeBucket>>,
    external_directories: Option<ExternalDirectoryOverview>,
}

impl AnalysisReport {
    #[must_use]
    pub fn detailed(
        space: SpaceReport,
        row_counts: BTreeMap<String, u64>,
        attribution: AttributionReport,
        orphans: OrphanReport,
        distribution: DistributionReport,
    ) -> Self {
        Self {
            mode: ReportMode::Detailed,
            file_space: space.file,
            row_counts: Some(row_counts),
            table_space: Some(space.objects),
            project_attribution: Some(attribution.projects),
            largest_sessions: Some(attribution.sessions),
            orphans: Some(orphans),
            age_distribution: Some(distribution.age_buckets),
            external_directories: Some(distribution.external),
        }
    }

    #[must_use]
    pub fn standard(
        file_space: FileSpace,
        attribution: AttributionReport,
        age_distribution: Vec<AgeBucket>,
    ) -> Self {
        Self {
            mode: ReportMode::Standard,
            file_space,
            row_counts: None,
            table_space: None,
            project_attribution: Some(attribution.projects),
            largest_sessions: Some(attribution.sessions),
            orphans: None,
            age_distribution: Some(age_distribution),
            external_directories: None,
        }
    }
}

/// Writes the aligned terminal report.
///
/// # Errors
///
/// Returns [`Error::Io`] when writing fails.
pub fn write_human(
    report: &AnalysisReport,
    output: &mut dyn Write,
    style: format::Style,
) -> Result<(), Error> {
    human::write(report, output, style)
}

/// Writes one schema-versioned JSON object.
///
/// # Errors
///
/// Returns [`Error::Io`] when serialization or writing fails.
pub fn write_json(report: &AnalysisReport, output: &mut dyn Write) -> Result<(), Error> {
    json::write(report, output)
}
