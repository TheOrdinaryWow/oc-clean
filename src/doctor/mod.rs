//! Read-only database health diagnostics.

pub mod command;

mod checks;
mod human;
mod json;
mod model;

pub use model::DoctorReport;

const SCHEMA_VERSION: u32 = 1;
