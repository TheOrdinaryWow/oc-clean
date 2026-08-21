pub mod types;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use self::types::{Duration, Size};

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
    /// Delete selected sessions and reclaim their database space.
    Clean(CleanArgs),
    /// Reclaim database freelist space, using a dry-run unless --apply is supplied.
    Vacuum(VacuumArgs),
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct CleanArgs {
    /// Select sessions whose complete subtree has been inactive for this age.
    #[arg(long, value_name = "AGE")]
    pub older_than: Option<Duration>,

    /// Select every session belonging to a matching project path or glob.
    #[arg(long, value_name = "PATH_OR_GLOB")]
    pub project: Option<String>,

    /// Select sessions whose complete subtree payload reaches this size.
    #[arg(long, value_name = "SIZE")]
    pub larger_than: Option<Size>,

    /// Select archived sessions.
    #[arg(long)]
    pub archived: bool,

    /// Include database and external-storage orphans.
    #[arg(long)]
    pub orphans: bool,

    /// Retain this many most recently active root sessions per project.
    #[arg(long, default_value_t = 100, value_name = "N")]
    pub keep_recent: u64,

    /// Use incremental auto-vacuum instead of rebuilding the database.
    #[arg(long)]
    pub incremental: bool,

    /// Commit deletions without reclaiming database pages.
    #[arg(long)]
    pub no_vacuum: bool,

    /// Compact retained snapshot repositories after cleanup.
    #[arg(long)]
    pub gc_snapshots: bool,

    /// Emit the report as JSON on stdout.
    #[arg(long)]
    pub json: bool,

    /// Select the tracing diagnostic format written to stderr.
    #[arg(long, value_enum, default_value_t = LogFormat::Text)]
    pub log_format: LogFormat,
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands, LogFormat};

    #[test]
    fn clean_accepts_every_command_specific_option() {
        let cli = Cli::try_parse_from([
            "oc-clean",
            "--db",
            "/tmp/opencode.db",
            "--apply",
            "clean",
            "--older-than",
            "30d",
            "--project",
            "/work/*",
            "--larger-than",
            "10MB",
            "--archived",
            "--orphans",
            "--keep-recent",
            "3",
            "--incremental",
            "--no-vacuum",
            "--gc-snapshots",
            "--json",
            "--log-format",
            "json",
        ])
        .expect("clean arguments should parse");

        let Commands::Clean(arguments) = cli.command else {
            panic!("clean command should parse");
        };
        assert_eq!(
            arguments.older_than.expect("age").as_millis() / 1_000,
            30 * 86_400
        );
        assert_eq!(arguments.project.as_deref(), Some("/work/*"));
        assert_eq!(arguments.larger_than.expect("size").as_bytes(), 10_000_000);
        assert!(arguments.archived);
        assert!(arguments.orphans);
        assert_eq!(arguments.keep_recent, 3);
        assert!(arguments.incremental);
        assert!(arguments.no_vacuum);
        assert!(arguments.gc_snapshots);
        assert!(arguments.json);
        assert_eq!(arguments.log_format, LogFormat::Json);
    }

    #[test]
    fn clean_defaults_to_one_hundred_recent_sessions_per_project() {
        let cli = Cli::try_parse_from(["oc-clean", "clean"]).expect("clean should parse");
        let Commands::Clean(arguments) = cli.command else {
            panic!("clean command should parse");
        };

        assert_eq!(arguments.keep_recent, 100);
    }
}
