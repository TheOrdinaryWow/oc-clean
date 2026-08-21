//! Read-only database health diagnostics.

pub mod command;

mod checks;
mod human;
mod json;
mod model;

#[cfg(test)]
mod tests;

pub(crate) use checks::{foreign_key_check, integrity_check};
pub use model::DoctorReport;

const SCHEMA_VERSION: u32 = 1;
