pub mod types;

use std::path::PathBuf;

use clap::builder::BoolishValueParser;
use clap::{Args, Parser, Subcommand, ValueEnum};

use self::types::{Duration, Size};

/// Value parser for every switch that can also be set from the environment.
///
/// A command line writes `--flag` and never a value, but an environment variable has to spell
/// one out, and there is no single spelling operators agree on: shell scripts export `1`, CI
/// systems write `true`, and configuration files use `yes` or `on`. clap's default boolean
/// parser accepts only `true` and `false`, which rejects the rest with a confusing message about
/// a flag the user never typed a value for.
///
/// `BoolishValueParser` accepts `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n`, `t`/`f`, and
/// `on`/`off`, case-insensitively. Only `true` and `false` are advertised in `--help`, so the
/// help output stays as short as it is now.
fn boolish() -> BoolishValueParser {
    BoolishValueParser::new()
}

#[derive(Debug, Parser)]
#[command(
    name = "oc-clean",
    version,
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

    /// Select the `OpenCode` release channel used for the default database filename.
    #[arg(long, env = "OCC_CHANNEL", value_name = "NAME", global = true)]
    pub channel: Option<String>,

    /// Select the diagnostic log destination written to stderr.
    ///
    /// Logging stays off by default so human reports and progress rendering remain readable.
    /// Setting `RUST_LOG` implicitly enables text logging while this option stays unset.
    #[arg(
        long,
        env = "OCC_LOG",
        value_enum,
        default_value_t = LogMode::Off,
        value_name = "MODE",
        global = true
    )]
    pub log: LogMode,

    /// Preview the selection and exit without mutating anything.
    #[arg(long, env = "OCC_DRY_RUN", global = true, value_parser = boolish())]
    pub dry_run: bool,

    #[arg(long, global = true)]
    pub force: bool,

    #[arg(long, global = true)]
    pub force_schema: bool,

    #[arg(long, global = true)]
    pub dangerously_skip_confirm: bool,

    #[arg(long, global = true)]
    pub skip_backup: bool,
}

impl Cli {
    /// Reports whether the selected subcommand emits a JSON report on stdout.
    #[must_use]
    pub const fn json(&self) -> bool {
        match &self.command {
            Commands::Analyze(arguments) => arguments.json,
            Commands::Doctor(arguments) => arguments.json,
            Commands::Clean(arguments) => arguments.json,
            Commands::Vacuum(arguments) => arguments.json,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Analyze database and external storage usage without modifying either.
    Analyze(AnalyzeArgs),
    /// Diagnose schema, integrity, holders, and reclaim readiness without modifying the database.
    Doctor(DoctorArgs),
    /// Delete selected sessions and reclaim their database space.
    Clean(CleanArgs),
    /// Reclaim database freelist space after an interactive confirmation.
    Vacuum(VacuumArgs),
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct CleanArgs {
    /// Select sessions whose complete subtree has been inactive for this age.
    #[arg(long, env = "OCC_OLDER_THAN", value_name = "AGE")]
    pub older_than: Option<Duration>,

    /// Select every session belonging to a matching project path or glob.
    #[arg(long, env = "OCC_INCLUDE", value_name = "PATH_OR_GLOB")]
    pub include: Option<String>,

    /// Select every session whose project path or glob does not match.
    #[arg(
        long,
        env = "OCC_EXCLUDE",
        value_name = "PATH_OR_GLOB",
        conflicts_with = "include"
    )]
    pub exclude: Option<String>,

    /// Select sessions whose complete subtree payload reaches this size.
    #[arg(long, env = "OCC_LARGER_THAN", value_name = "SIZE")]
    pub larger_than: Option<Size>,

    /// Select archived sessions.
    #[arg(long, env = "OCC_ARCHIVED", value_parser = boolish())]
    pub archived: bool,

    /// Include database and external-storage orphans.
    #[arg(long, env = "OCC_ORPHANS", value_parser = boolish())]
    pub orphans: bool,

    /// Retain this many most recently active root sessions per project.
    #[arg(long, env = "OCC_KEEP_RECENT", default_value_t = 0, value_name = "N")]
    pub keep_recent: u64,

    /// Use incremental auto-vacuum instead of rebuilding the database.
    #[arg(long, env = "OCC_INCREMENTAL", value_parser = boolish())]
    pub incremental: bool,

    /// Commit deletions without reclaiming database pages.
    #[arg(long, env = "OCC_NO_VACUUM", value_parser = boolish())]
    pub no_vacuum: bool,

    /// Compact retained snapshot repositories after cleanup.
    #[arg(long, env = "OCC_GC_SNAPSHOTS", value_parser = boolish())]
    pub gc_snapshots: bool,

    /// Also prune projects that were already empty before this cleanup.
    #[arg(long, env = "OCC_PRUNE_EMPTY_PROJECTS", value_parser = boolish())]
    pub prune_empty_projects: bool,

    /// Limit the selected-session preview listed before deletion.
    #[arg(long, env = "OCC_TOP", default_value_t = 10, value_name = "N")]
    pub top: usize,

    /// Emit the report as JSON on stdout.
    #[arg(long, env = "OCC_JSON", value_parser = boolish())]
    pub json: bool,
}

/// Diagnostic logging destination selected by `--log`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum LogMode {
    /// Suppress tracing diagnostics entirely.
    #[default]
    Off,
    /// Write human-readable tracing diagnostics to stderr.
    Text,
    /// Write structured JSON tracing diagnostics to stderr.
    Json,
}

impl LogMode {
    /// Reports whether any tracing diagnostics should reach stderr.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// Emit the report as one stable JSON object on stdout.
    #[arg(long, env = "OCC_JSON", value_parser = boolish())]
    pub json: bool,

    /// Limit the largest-session rollup.
    #[arg(long, env = "OCC_TOP", default_value_t = 10, value_name = "N")]
    pub top: usize,

    /// Add the technical layers: orphan census, external directories, object space, row counts.
    #[arg(long, env = "OCC_DETAILED", value_parser = boolish())]
    pub detailed: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit the report as one stable JSON object on stdout.
    #[arg(long, env = "OCC_JSON", value_parser = boolish())]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct VacuumArgs {
    /// Emit the report as one stable JSON object on stdout.
    #[arg(long, env = "OCC_JSON", value_parser = boolish())]
    pub json: bool,

    /// Reclaim freelist pages from a database already using incremental auto-vacuum.
    #[arg(long, env = "OCC_INCREMENTAL", value_parser = boolish())]
    pub incremental: bool,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clap::{Command, CommandFactory, Parser};

    use super::{Cli, Commands, LogMode};

    #[test]
    fn clean_accepts_every_command_specific_option() {
        let cli = Cli::try_parse_from([
            "oc-clean",
            "--db",
            "/tmp/opencode.db",
            "--dry-run",
            "--log",
            "json",
            "clean",
            "--older-than",
            "30d",
            "--include",
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
            "--prune-empty-projects",
            "--top",
            "7",
            "--json",
        ])
        .expect("clean arguments should parse");

        assert_eq!(cli.log, LogMode::Json);
        let Commands::Clean(arguments) = cli.command else {
            panic!("clean command should parse");
        };
        assert_eq!(
            arguments.older_than.expect("age").as_millis() / 1_000,
            30 * 86_400
        );
        assert_eq!(arguments.include.as_deref(), Some("/work/*"));
        assert_eq!(arguments.exclude, None);
        assert_eq!(arguments.larger_than.expect("size").as_bytes(), 10_000_000);
        assert!(arguments.archived);
        assert!(arguments.orphans);
        assert_eq!(arguments.keep_recent, 3);
        assert!(arguments.incremental);
        assert!(arguments.no_vacuum);
        assert!(arguments.gc_snapshots);
        assert!(arguments.prune_empty_projects);
        assert_eq!(arguments.top, 7);
        assert!(arguments.json);
    }

    #[test]
    fn logging_is_off_unless_requested() {
        let cli = Cli::try_parse_from(["oc-clean", "analyze"]).expect("analyze should parse");
        assert_eq!(cli.log, LogMode::Off);
        assert!(!cli.log.is_enabled());
    }

    #[test]
    fn log_option_is_global_for_every_subcommand() {
        for subcommand in ["analyze", "doctor", "clean", "vacuum"] {
            let cli = Cli::try_parse_from(["oc-clean", subcommand, "--log", "text"])
                .expect("global --log should parse after every subcommand");
            assert_eq!(cli.log, LogMode::Text);
            assert!(cli.log.is_enabled());
        }
    }

    #[test]
    fn every_non_destructive_option_has_an_occ_environment_binding() {
        const DESTRUCTIVE_OPTIONS: [&str; 4] = [
            "force",
            "force-schema",
            "dangerously-skip-confirm",
            "skip-backup",
        ];

        fn assert_bindings(command: &Command, checked: &mut BTreeSet<String>) {
            for argument in command.get_arguments() {
                let Some(long) = argument.get_long() else {
                    continue;
                };
                // `--help` and `--version` are generated by clap, not authored options.
                if long == "help" || long == "version" {
                    continue;
                }
                if !checked.insert(long.to_owned()) {
                    continue;
                }

                if DESTRUCTIVE_OPTIONS.contains(&long) {
                    assert_eq!(
                        argument.get_env(),
                        None,
                        "destructive option --{long} must require explicit CLI input"
                    );
                } else {
                    let expected = format!("OCC_{}", long.replace('-', "_").to_uppercase());
                    assert_eq!(
                        argument.get_env().and_then(std::ffi::OsStr::to_str),
                        Some(expected.as_str()),
                        "option --{long} must use {expected}"
                    );
                }
            }
            for subcommand in command.get_subcommands() {
                assert_bindings(subcommand, checked);
            }
        }

        let mut command = Cli::command();
        command.build();
        let mut checked = BTreeSet::new();
        assert_bindings(&command, &mut checked);

        assert!(
            DESTRUCTIVE_OPTIONS
                .iter()
                .all(|option| checked.contains(*option))
        );
    }

    #[test]
    fn boolean_environment_bindings_accept_the_shapes_operators_actually_write() {
        // Values a shell script or CI system is likely to export for an on/off switch. Parsing
        // is exercised end-to-end in the environment-binding integration test; this checks that
        // every switch advertises the same accepted set, so none is left on clap's default
        // `true`/`false`-only parser.
        const ACCEPTED: [&str; 8] = ["1", "0", "true", "false", "yes", "no", "on", "off"];

        fn assert_accepts(command: &Command, checked: &mut BTreeSet<String>) {
            for argument in command.get_arguments() {
                let Some(long) = argument.get_long() else {
                    continue;
                };
                if argument.get_env().is_none() || !checked.insert(long.to_owned()) {
                    continue;
                }
                // Only the on/off switches take a boolean-shaped value.
                if !matches!(argument.get_action(), clap::ArgAction::SetTrue) {
                    continue;
                }

                let possible = argument
                    .get_value_parser()
                    .possible_values()
                    .map(|values| {
                        values
                            .map(|value| value.get_name().to_owned())
                            .collect::<BTreeSet<_>>()
                    })
                    .unwrap_or_default();
                for value in ACCEPTED {
                    assert!(
                        possible.contains(value),
                        "--{long} should accept `{value}`, accepts {possible:?}"
                    );
                }
            }
            for subcommand in command.get_subcommands() {
                assert_accepts(subcommand, checked);
            }
        }

        let mut command = Cli::command();
        command.build();
        let mut checked = BTreeSet::new();
        assert_accepts(&command, &mut checked);
        assert!(
            checked.contains("detailed"),
            "the walk should have reached the subcommand flags"
        );
    }

    #[test]
    fn clean_retains_nothing_unless_keep_recent_is_requested() {
        let cli = Cli::try_parse_from(["oc-clean", "clean"]).expect("clean should parse");
        let Commands::Clean(arguments) = cli.command else {
            panic!("clean command should parse");
        };

        assert_eq!(arguments.keep_recent, 0);
        assert_eq!(arguments.top, 10);
    }

    #[test]
    fn version_reports_the_package_version() {
        let error = Cli::try_parse_from(["oc-clean", "--version"])
            .expect_err("--version exits through clap rather than producing a Cli");

        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn include_and_exclude_are_mutually_exclusive() {
        let included = Cli::try_parse_from(["oc-clean", "clean", "--include", "/work/*"])
            .expect("include alone should parse");
        let excluded = Cli::try_parse_from(["oc-clean", "clean", "--exclude", "/work/*"])
            .expect("exclude alone should parse");
        let Commands::Clean(included) = included.command else {
            panic!("clean command should parse");
        };
        let Commands::Clean(excluded) = excluded.command else {
            panic!("clean command should parse");
        };

        assert_eq!(included.include.as_deref(), Some("/work/*"));
        assert_eq!(excluded.exclude.as_deref(), Some("/work/*"));

        let conflict =
            Cli::try_parse_from(["oc-clean", "clean", "--include", "/a", "--exclude", "/b"])
                .expect_err("include and exclude must not be combined");
        assert_eq!(conflict.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn destructive_work_is_the_default_and_dry_run_is_opt_in() {
        let default = Cli::try_parse_from(["oc-clean", "clean"]).expect("clean should parse");
        let previewed =
            Cli::try_parse_from(["oc-clean", "clean", "--dry-run"]).expect("clean should parse");

        assert!(!default.dry_run);
        assert!(previewed.dry_run);
        assert!(Cli::try_parse_from(["oc-clean", "clean", "--apply"]).is_err());
    }
}
