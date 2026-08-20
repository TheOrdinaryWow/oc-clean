//! Stable report model and stdout renderers for analysis commands.

pub mod command;
mod human;
pub mod impact;
mod json;
pub mod logging;

use std::collections::BTreeMap;
use std::io::Write;

use crate::analyze::attribution::{AttributionReport, ProjectAttribution, SessionAttribution};
use crate::analyze::distribution::{AgeBucket, DistributionReport, ExternalDirectoryOverview};
use crate::analyze::orphans::OrphanReport;
use crate::analyze::space::{FileSpace, ObjectSpaceReport, SpaceReport};
use crate::error::Error;

/// Version of the documented JSON report contract.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportMode {
    Full,
    Quick,
}

/// Normalized report consumed by both renderers.
///
/// JSON always includes `schema_version`, `mode`, `file_space`, `row_counts`, and all seven layer
/// keys. Full reports populate every layer. Quick reports set scan-backed layers to `null`.
#[derive(Clone, Debug)]
pub struct AnalysisReport {
    mode: ReportMode,
    file_space: FileSpace,
    row_counts: BTreeMap<String, u64>,
    table_space: Option<ObjectSpaceReport>,
    project_attribution: Option<Vec<ProjectAttribution>>,
    largest_sessions: Option<Vec<SessionAttribution>>,
    orphans: Option<OrphanReport>,
    age_distribution: Option<Vec<AgeBucket>>,
    external_directories: Option<ExternalDirectoryOverview>,
}

impl AnalysisReport {
    #[must_use]
    pub fn full(
        space: SpaceReport,
        row_counts: BTreeMap<String, u64>,
        attribution: AttributionReport,
        orphans: OrphanReport,
        distribution: DistributionReport,
    ) -> Self {
        Self {
            mode: ReportMode::Full,
            file_space: space.file,
            row_counts,
            table_space: Some(space.objects),
            project_attribution: Some(attribution.projects),
            largest_sessions: Some(attribution.sessions),
            orphans: Some(orphans),
            age_distribution: Some(distribution.age_buckets),
            external_directories: Some(distribution.external),
        }
    }

    #[must_use]
    pub fn quick(file_space: FileSpace, row_counts: BTreeMap<String, u64>) -> Self {
        Self {
            mode: ReportMode::Quick,
            file_space,
            row_counts,
            table_space: None,
            project_attribution: None,
            largest_sessions: None,
            orphans: None,
            age_distribution: None,
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
    color: bool,
) -> Result<(), Error> {
    human::write(report, output, color)
}

/// Writes one schema-versioned JSON object.
///
/// # Errors
///
/// Returns [`Error::Io`] when serialization or writing fails.
pub fn write_json(report: &AnalysisReport, output: &mut dyn Write) -> Result<(), Error> {
    json::write(report, output)
}
