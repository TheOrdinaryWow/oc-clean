use std::path::PathBuf;

use crate::analyze::orphans::OrphanReport;
use crate::db::schema::SchemaReport;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckReport {
    pub ok: bool,
    pub findings: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForeignKeyFinding {
    pub table: String,
    pub row_id: Option<i64>,
    pub parent_table: String,
    pub foreign_key_index: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForeignKeyReport {
    pub ok: bool,
    pub findings: Vec<ForeignKeyFinding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HolderProcess {
    pub pid: u32,
    pub name: Option<String>,
    pub observed_via: &'static str,
    pub matched_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HolderReport {
    pub verdict: &'static str,
    pub reason: Option<String>,
    pub completeness: &'static str,
    pub completeness_discriminant: &'static str,
    pub processes: Vec<HolderProcess>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VacuumHeadroom {
    pub estimated_required_bytes: u64,
    pub available_bytes: u64,
    pub vacuum_into_feasible: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoVacuumReport {
    pub value: u32,
    pub mode: &'static str,
    pub incremental_applicable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimestampSanity {
    pub max_time_updated: Option<i64>,
    pub ok: bool,
    pub classification: &'static str,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorReport {
    pub database_path: PathBuf,
    pub schema: SchemaReport,
    pub integrity_check: CheckReport,
    pub foreign_key_check: ForeignKeyReport,
    pub orphans: OrphanReport,
    pub holders: HolderReport,
    pub vacuum_headroom: VacuumHeadroom,
    pub auto_vacuum: AutoVacuumReport,
    pub timestamp_sanity: TimestampSanity,
}
