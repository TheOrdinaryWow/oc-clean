pub mod types;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "oc-clean",
    about = "Prune old OpenCode data and reclaim disk space"
)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "CLI switches are independent user choices"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    #[arg(long, env = "OCC_DB", value_name = "PATH", global = true)]
    pub db: Option<PathBuf>,

    #[arg(long, global = true)]
    pub apply: bool,

    #[arg(long, global = true)]
    pub force: bool,

    #[arg(long, global = true)]
    pub force_schema: bool,

    #[arg(long, global = true)]
    pub dangerously_skip_confirm: bool,

    #[arg(long, global = true)]
    pub skip_backup: bool,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Analyze database and external storage usage without modifying either.
    Analyze(AnalyzeArgs),
    /// Diagnose schema, integrity, holders, and reclaim readiness without modifying the database.
    Doctor(DoctorArgs),
    /// Reclaim database freelist space, using a dry-run unless --apply is supplied.
    Vacuum(VacuumArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// Emit the report as one stable JSON object on stdout.
    #[arg(long)]
    pub json: bool,

    /// Select the tracing diagnostic format written to stderr.
    #[arg(long, value_enum, default_value_t = LogFormat::Text)]
    pub log_format: LogFormat,

    /// Limit the largest-session rollup.
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub top: usize,

    /// Emit only file-level accounting and row counts.
    #[arg(long)]
    pub quick: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit the report as one stable JSON object on stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct VacuumArgs {
    /// Emit the report as one stable JSON object on stdout.
    #[arg(long)]
    pub json: bool,

    /// Select the tracing diagnostic format written to stderr.
    #[arg(long, value_enum, default_value_t = LogFormat::Text)]
    pub log_format: LogFormat,

    /// Reclaim freelist pages from a database already using incremental auto-vacuum.
    #[arg(long)]
    pub incremental: bool,
}
