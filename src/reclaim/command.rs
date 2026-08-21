use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde_json::json;
use tracing::{info, info_span, warn};
use tracing_indicatif::span_ext::IndicatifSpanExt;

#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../clean/signal.rs"]
mod signal;

use super::headroom::{
    FreeSpaceProvider, Fs2FreeSpaceProvider, HeadroomEstimate, HeadroomInput, HeadroomVerdict,
    evaluate_headroom,
};
use super::incremental::{
    DEFAULT_PAGES_PER_BATCH, IncrementalVacuumError, check_auto_vacuum, incremental_vacuum,
};
use super::vacuum_into::{VacuumIntoOptions, vacuum_into_with_observer};
use crate::analyze::space;
use crate::cli::{Cli, VacuumArgs};
use crate::db::{self, ConnectionOptions};
use crate::error::Error;
use crate::paths::{self, DatabaseOptions, Environment, Platform, Target};
use crate::safety::confirm::{ConfirmationDecision, ConfirmationOptions, ImpactSummary, confirm};
use crate::safety::holders::{CommandMode, GateDecision, HolderInspector, inspect_and_decide};

use signal::SignalController;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RuntimeContext {
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
}

impl RuntimeContext {
    #[cfg(test)]
    const fn interactive() -> Self {
        Self {
            stdin_is_terminal: true,
            stdout_is_terminal: true,
        }
    }

    #[cfg(test)]
    const fn piped() -> Self {
        Self {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Strategy {
    VacuumInto,
    Incremental,
}

impl Strategy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::VacuumInto => "vacuum-into",
            Self::Incremental => "incremental",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct VacuumReport {
    strategy: Strategy,
    applied: bool,
    current_size: u64,
    live_bytes: u64,
    freelist_bytes: u64,
    estimated_post_vacuum_size: u64,
    bytes_reclaimed: Option<u64>,
    headroom: Option<HeadroomEstimate>,
}

/// Runs a vacuum preview or application and writes the selected report format.
///
/// # Errors
///
/// Returns typed path, schema, holder, headroom, confirmation, SQLite, or output failures.
pub fn run(cli: &Cli, arguments: &VacuumArgs, output: &mut dyn Write) -> Result<(), Error> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let signals = SignalController::new();
    signals.install()?;
    let runtime = RuntimeContext {
        stdin_is_terminal: stdin.is_terminal(),
        stdout_is_terminal: io::stdout().is_terminal(),
    };
    let inspector = platform_inspector();
    run_with(
        cli,
        arguments,
        &mut input,
        output,
        &Fs2FreeSpaceProvider,
        inspector.as_ref(),
        runtime,
        &signals,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with<R, W, P>(
    cli: &Cli,
    arguments: &VacuumArgs,
    input: &mut R,
    output: &mut W,
    free_space: &P,
    holder_inspector: &dyn HolderInspector,
    runtime: RuntimeContext,
    signals: &SignalController,
) -> Result<(), Error>
where
    R: BufRead + ?Sized,
    W: Write + ?Sized,
    P: FreeSpaceProvider,
{
    let target = database_target(cli)?;
    let database_path = file_path(&target)?;
    let database = db::open_read_only(&target, ConnectionOptions::default())?;
    db::schema::inspect(database.connection(), cli.force_schema)?;
    let file_space = space::analyze(&database)?.file;
    let current_size = database_path
        .metadata()
        .map_err(|source| io_error(database_path, source))?
        .len();
    let hardlink_supported = database.capabilities().hard_links;
    drop(database);

    let (_, holder_decision) = inspect_and_decide(
        holder_inspector,
        database_path,
        CommandMode::Vacuum { apply: cli.apply },
        cli.force,
    );
    if matches!(holder_decision, GateDecision::Warn) {
        warn!("database holder state requires attention");
    }
    holder_decision.into_result()?;

    let report = VacuumReport {
        strategy: if arguments.incremental {
            Strategy::Incremental
        } else {
            Strategy::VacuumInto
        },
        applied: cli.apply,
        current_size,
        live_bytes: file_space.live_bytes,
        freelist_bytes: file_space.freelist_bytes,
        estimated_post_vacuum_size: file_space.live_bytes,
        bytes_reclaimed: None,
        headroom: None,
    };

    if arguments.incremental {
        run_incremental(
            cli, arguments, input, output, runtime, &target, report, signals,
        )
    } else {
        run_vacuum_into(
            cli,
            arguments,
            input,
            output,
            free_space,
            runtime,
            &target,
            database_path,
            hardlink_supported,
            report,
            signals,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn run_incremental<R, W>(
    cli: &Cli,
    arguments: &VacuumArgs,
    input: &mut R,
    output: &mut W,
    runtime: RuntimeContext,
    target: &Target,
    mut report: VacuumReport,
    signals: &SignalController,
) -> Result<(), Error>
where
    R: BufRead + ?Sized,
    W: Write + ?Sized,
{
    let database = db::open_read_write(target, ConnectionOptions::default())?;
    check_auto_vacuum(&database).map_err(incremental_error)?;
    if !cli.apply {
        return write_report(arguments, &report, output);
    }
    ensure_confirmed(cli, arguments, input, output, runtime, &report)?;
    signals.set_interrupt_handle(database.interrupt_handle());
    signals.begin_reclaim();
    let progress_span = info_span!("incremental vacuum");
    progress_span.pb_set_length(report.freelist_bytes);
    progress_span.pb_set_message("reclaiming freelist pages");
    progress_span.pb_start();
    let _entered = progress_span.enter();
    let vacuum_report = incremental_vacuum(
        &database,
        DEFAULT_PAGES_PER_BATCH,
        || signals.cancelled(),
        |progress| {
            progress_span.pb_set_position(progress.bytes_reclaimed);
            info!(
                pages_reclaimed = progress.pages_reclaimed,
                bytes_reclaimed = progress.bytes_reclaimed,
                "incremental vacuum progress"
            );
        },
    );
    let vacuum_report = match vacuum_report {
        Ok(_) | Err(_) if signals.cancelled() => return Err(interrupted("incremental vacuum")),
        Ok(report) => report,
        Err(error) => return Err(incremental_error(error)),
    };
    report.bytes_reclaimed = Some(vacuum_report.bytes_reclaimed);
    write_report(arguments, &report, output)
}

#[allow(clippy::too_many_arguments)]
fn run_vacuum_into<R, W, P>(
    cli: &Cli,
    arguments: &VacuumArgs,
    input: &mut R,
    output: &mut W,
    free_space: &P,
    runtime: RuntimeContext,
    target: &Target,
    database_path: &Path,
    hardlink_supported: bool,
    mut report: VacuumReport,
    signals: &SignalController,
) -> Result<(), Error>
where
    R: BufRead + ?Sized,
    W: Write + ?Sized,
    P: FreeSpaceProvider,
{
    let headroom = evaluate_headroom(
        free_space,
        database_path,
        HeadroomInput {
            current_live_bytes: report.live_bytes,
            selected_session_bytes: 0,
            full_original_size: report.current_size,
            one_batch_wal_allowance: 0,
            margin_fraction: HeadroomInput::DEFAULT_MARGIN_FRACTION,
            hardlink_supported,
        },
    )
    .map_err(|source| io_error(database_path, source))?;
    report.headroom = Some(headroom);
    if let HeadroomVerdict::InsufficientWithShortfall { shortfall_bytes } = headroom.verdict {
        write_headroom_refusal(arguments, headroom, shortfall_bytes, output)?;
        return Err(Error::InsufficientDiskSpace {
            required_bytes: headroom.required_bytes,
            available_bytes: headroom.available_bytes,
        });
    }
    if !cli.apply {
        return write_report(arguments, &report, output);
    }
    ensure_confirmed(cli, arguments, input, output, runtime, &report)?;
    let database = db::open_read_write(target, ConnectionOptions::default())?;
    signals.set_interrupt_handle(database.interrupt_handle());
    signals.begin_reclaim();
    if signals.cancelled() {
        return Err(interrupted("zero database mutations"));
    }
    database.acquire_exclusive_lock()?;
    let data_version = database.data_version()?;
    let vacuum_report = vacuum_into_with_observer(
        database,
        database_path,
        data_version,
        VacuumIntoOptions {
            skip_backup: cli.skip_backup,
        },
        signals,
    );
    let vacuum_report = match vacuum_report {
        Ok(_) if signals.cancelled() => return Err(interrupted("atomic database swap")),
        Ok(report) => report,
        Err(_) if signals.cancelled() => return Err(interrupted("VACUUM output cleanup")),
        Err(error) => return Err(error),
    };
    report.bytes_reclaimed = Some(vacuum_report.bytes_reclaimed);
    report.estimated_post_vacuum_size = vacuum_report.compacted_bytes;
    write_report(arguments, &report, output)
}

fn ensure_confirmed<R, W>(
    cli: &Cli,
    arguments: &VacuumArgs,
    input: &mut R,
    output: &mut W,
    runtime: RuntimeContext,
    report: &VacuumReport,
) -> Result<(), Error>
where
    R: BufRead + ?Sized,
    W: Write + ?Sized,
{
    let details = format!(
        "Strategy: {}; current size: {} bytes; estimated post-vacuum size: {} bytes",
        report.strategy.as_str(),
        report.current_size,
        report.estimated_post_vacuum_size
    );
    let decision = confirm(
        &ImpactSummary {
            operation: "Vacuum",
            details: &details,
        },
        ConfirmationOptions {
            stdin_is_terminal: runtime.stdin_is_terminal,
            stdout_is_terminal: runtime.stdout_is_terminal,
            json: arguments.json,
            dangerously_skip_confirm: cli.dangerously_skip_confirm,
        },
        input,
        output,
    )
    .map_err(|source| io_error(Path::new("<terminal>"), source))?;
    match decision {
        ConfirmationDecision::Proceed => Ok(()),
        ConfirmationDecision::Refuse => Err(Error::InvalidArgument {
            argument: "--apply".to_owned(),
            reason:
                "vacuum application requires interactive confirmation or --dangerously-skip-confirm"
                    .to_owned(),
        }),
    }
}

fn write_report(
    arguments: &VacuumArgs,
    report: &VacuumReport,
    output: &mut (impl Write + ?Sized),
) -> Result<(), Error> {
    if arguments.json {
        let headroom = report.headroom.map(|estimate| {
            json!({
                "required_bytes": estimate.required_bytes,
                "available_bytes": estimate.available_bytes,
            })
        });
        serde_json::to_writer_pretty(
            &mut *output,
            &json!({
                "schema_version": 1,
                "strategy": report.strategy.as_str(),
                "mode": if report.applied { "applied" } else { "dry-run" },
                "current_size": report.current_size,
                "live_bytes": report.live_bytes,
                "freelist_bytes": report.freelist_bytes,
                "estimated_post_vacuum_size": report.estimated_post_vacuum_size,
                "bytes_reclaimed": report.bytes_reclaimed,
                "headroom": headroom,
            }),
        )
        .map_err(json_error)?;
        writeln!(output).map_err(output_error)
    } else {
        writeln!(
            output,
            "Vacuum ({})",
            if report.applied { "applied" } else { "dry-run" }
        )
        .map_err(output_error)?;
        writeln!(output, "  Strategy: {}", report.strategy.as_str()).map_err(output_error)?;
        writeln!(output, "  Current size: {} bytes", report.current_size).map_err(output_error)?;
        writeln!(output, "  Live bytes: {}", report.live_bytes).map_err(output_error)?;
        writeln!(output, "  Freelist bytes: {}", report.freelist_bytes).map_err(output_error)?;
        writeln!(
            output,
            "  Estimated post-vacuum size: {} bytes",
            report.estimated_post_vacuum_size
        )
        .map_err(output_error)?;
        if let Some(bytes_reclaimed) = report.bytes_reclaimed {
            writeln!(output, "  Bytes reclaimed: {bytes_reclaimed}").map_err(output_error)?;
        }
        Ok(())
    }
}

fn write_headroom_refusal(
    arguments: &VacuumArgs,
    headroom: HeadroomEstimate,
    shortfall_bytes: u64,
    output: &mut (impl Write + ?Sized),
) -> Result<(), Error> {
    if arguments.json {
        serde_json::to_writer_pretty(
            &mut *output,
            &json!({
                "schema_version": 1,
                "error": "insufficient-disk-space",
                "required_bytes": headroom.required_bytes,
                "available_bytes": headroom.available_bytes,
                "bytes_to_free": shortfall_bytes,
            }),
        )
        .map_err(json_error)?;
        writeln!(output).map_err(output_error)
    } else {
        writeln!(output, "Insufficient disk space").map_err(output_error)?;
        writeln!(output, "  Required bytes: {}", headroom.required_bytes).map_err(output_error)?;
        writeln!(output, "  Available bytes: {}", headroom.available_bytes)
            .map_err(output_error)?;
        writeln!(
            output,
            "  Free at least {shortfall_bytes} bytes before retrying"
        )
        .map_err(output_error)
    }
}

fn incremental_error(error: IncrementalVacuumError) -> Error {
    match error {
        IncrementalVacuumError::Cancelled { progress } => Error::Interrupted {
            completed: format!(
                "{} pages and {} bytes reclaimed",
                progress.pages_reclaimed, progress.bytes_reclaimed
            ),
        },
        IncrementalVacuumError::Sqlite { context, source } => Error::Sqlite {
            context: context.to_owned(),
            source,
        },
        other => Error::ReclaimUnavailable {
            reason: other.to_string(),
        },
    }
}

fn interrupted(completed: &str) -> Error {
    Error::Interrupted {
        completed: completed.to_owned(),
    }
}

fn database_target(cli: &Cli) -> Result<Target, Error> {
    let environment = Environment::from_iter(std::env::vars_os());
    paths::database_target(
        &environment,
        DatabaseOptions {
            explicit: cli.db.as_deref(),
            channel: None,
            platform: current_platform()?,
        },
    )
}

fn file_path(target: &Target) -> Result<&Path, Error> {
    match target {
        Target::File(path) => Ok(path),
        Target::Memory => Err(Error::InvalidArgument {
            argument: "--db".to_owned(),
            reason: "vacuum requires a file-backed database".to_owned(),
        }),
    }
}

fn current_platform() -> Result<Platform, Error> {
    match std::env::consts::OS {
        "linux" => Ok(Platform::Linux),
        "macos" => Ok(Platform::MacOs),
        "windows" => Ok(Platform::Windows),
        platform => Err(Error::UnsupportedPlatform {
            platform: platform.to_owned(),
        }),
    }
}

#[cfg(target_os = "linux")]
fn platform_inspector() -> Box<dyn HolderInspector> {
    Box::new(crate::safety::holders::linux::LinuxHolderInspector::default())
}

#[cfg(target_os = "macos")]
fn platform_inspector() -> Box<dyn HolderInspector> {
    Box::new(crate::safety::holders::macos::MacosHolderInspector)
}

#[cfg(windows)]
fn platform_inspector() -> Box<dyn HolderInspector> {
    Box::new(crate::safety::holders::windows::WindowsHolderInspector)
}

fn json_error(source: serde_json::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stdout>"),
        source: io::Error::other(source),
    }
}

fn output_error(source: io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stdout>"),
        source,
    }
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::fs;
    use std::hash::{DefaultHasher, Hash, Hasher};
    use std::io::{self, Cursor};
    use std::path::Path;
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use rusqlite::Connection;

    use super::*;
    use crate::cli::{Cli, Commands, LogFormat, VacuumArgs};
    use crate::reclaim::headroom::{FixedFreeSpaceProvider, FreeSpaceProvider};
    use crate::safety::holders::{Completeness, HolderInspector, Inspection, Verdict};

    use super::fixture::{Fixture, FixtureConfig};

    #[derive(Clone)]
    struct NotHeldInspector;

    impl HolderInspector for NotHeldInspector {
        fn inspect(&self, _database_path: &Path) -> Inspection {
            Inspection {
                verdict: Verdict::NotHeld,
                completeness: Completeness::CompleteForVisibleProcesses,
            }
        }
    }

    struct CountingFreeSpaceProvider {
        available_bytes: u64,
        calls: Cell<usize>,
    }

    impl CountingFreeSpaceProvider {
        fn new(available_bytes: u64) -> Self {
            Self {
                available_bytes,
                calls: Cell::new(0),
            }
        }
    }

    impl FreeSpaceProvider for CountingFreeSpaceProvider {
        fn available_space(&self, _directory: &Path) -> io::Result<u64> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.available_bytes)
        }
    }

    fn fixture() -> Fixture {
        Fixture::build(&FixtureConfig {
            session_count: 12,
            messages_per_session: 2,
            parts_per_message: 2,
            blob_size_per_part: 64 * 1_024,
            ..FixtureConfig::default()
        })
        .expect("fixture should build")
    }

    fn cli(path: &Path, apply: bool, dangerously_skip_confirm: bool) -> Cli {
        Cli {
            command: Commands::Vacuum(VacuumArgs {
                json: false,
                log_format: LogFormat::Text,
                incremental: false,
            }),
            db: Some(path.to_owned()),
            apply,
            force: false,
            force_schema: false,
            dangerously_skip_confirm,
            skip_backup: true,
        }
    }

    fn arguments(incremental: bool) -> VacuumArgs {
        VacuumArgs {
            json: false,
            log_format: LogFormat::Text,
            incremental,
        }
    }

    fn file_hash(path: &Path) -> u64 {
        let mut hasher = DefaultHasher::new();
        fs::read(path)
            .expect("fixture bytes should be readable")
            .hash(&mut hasher);
        hasher.finish()
    }

    fn table_counts(path: &Path) -> BTreeMap<String, u64> {
        let connection = Connection::open(path).expect("fixture should open");
        let mut tables = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .expect("table query should prepare");
        let names = tables
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table query should run")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("table names should decode");
        names
            .into_iter()
            .map(|name| {
                let quoted = name.replace('"', "\"\"");
                let count = connection
                    .query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .expect("table count should read");
                (
                    name,
                    u64::try_from(count).expect("table count should be non-negative"),
                )
            })
            .collect()
    }

    fn prepare_incremental_bloat(fixture: &Fixture) {
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .expect("incremental auto-vacuum should be requested");
        connection
            .execute_batch("VACUUM; DELETE FROM session;")
            .expect("fixture should enter incremental mode and gain free pages");
    }

    fn invoke(
        cli: &Cli,
        arguments: &VacuumArgs,
        provider: &impl FreeSpaceProvider,
        runtime: RuntimeContext,
    ) -> (Result<(), Error>, String) {
        let mut input = Cursor::new(b"yes\n");
        let mut output = Vec::new();
        let result = run_with(
            cli,
            arguments,
            &mut input,
            &mut output,
            provider,
            &NotHeldInspector,
            runtime,
            &SignalController::new(),
        );
        (
            result,
            String::from_utf8(output).expect("command output should be UTF-8"),
        )
    }

    #[test]
    fn dry_run_leaves_database_byte_identical_and_reports_space() {
        let fixture = fixture();
        let before = file_hash(&fixture.database_path);
        let cli = cli(&fixture.database_path, false, false);
        let (result, output) = invoke(
            &cli,
            &arguments(false),
            &FixedFreeSpaceProvider(u64::MAX),
            RuntimeContext::piped(),
        );

        result.expect("dry-run should succeed");
        assert_eq!(file_hash(&fixture.database_path), before);
        assert!(output.contains("Current size"));
        assert!(output.contains("Live bytes"));
        assert!(output.contains("Freelist bytes"));
        assert!(output.contains("Estimated post-vacuum size"));
    }

    #[test]
    fn insufficient_headroom_is_exit_six_in_interactive_and_piped_modes() {
        for runtime in [RuntimeContext::interactive(), RuntimeContext::piped()] {
            let fixture = fixture();
            let before = file_hash(&fixture.database_path);
            let cli = cli(&fixture.database_path, true, true);
            let (result, output) =
                invoke(&cli, &arguments(false), &FixedFreeSpaceProvider(1), runtime);

            let error = result.expect_err("low headroom should be refused");
            assert_eq!(error.exit_code(), 6);
            assert!(output.contains("Required bytes"));
            assert!(output.contains("Available bytes"));
            assert!(output.contains("Free at least"));
            assert!(!output.to_ascii_lowercase().contains("incremental"));
            assert_eq!(file_hash(&fixture.database_path), before);
        }
    }

    #[test]
    fn tier_one_incompatible_database_is_exit_four_before_reclaim() {
        let fixture = fixture();
        fixture
            .connect()
            .expect("fixture should connect")
            .execute_batch("ALTER TABLE session DROP COLUMN time_archived")
            .expect("required column should be removed");
        let before = file_hash(&fixture.database_path);
        let cli = cli(&fixture.database_path, true, true);
        let (result, _) = invoke(
            &cli,
            &arguments(false),
            &FixedFreeSpaceProvider(u64::MAX),
            RuntimeContext::piped(),
        );

        let error = result.expect_err("tier-one incompatibility should be refused");
        assert_eq!(error.exit_code(), 4);
        assert_eq!(file_hash(&fixture.database_path), before);
    }

    #[test]
    fn incremental_structurally_skips_headroom_while_default_refuses_same_fixture() {
        let fixture = fixture();
        prepare_incremental_bloat(&fixture);
        let provider = CountingFreeSpaceProvider::new(1);
        let cli = cli(&fixture.database_path, true, true);
        let (result, _) = invoke(&cli, &arguments(true), &provider, RuntimeContext::piped());
        result.expect("incremental vacuum should proceed with negligible free space");
        assert_eq!(provider.calls.get(), 0);

        let (result, _) = invoke(&cli, &arguments(false), &provider, RuntimeContext::piped());
        assert_eq!(
            result
                .expect_err("default vacuum should refuse")
                .exit_code(),
            6
        );
        assert_eq!(provider.calls.get(), 1);
    }

    #[test]
    fn incremental_unavailable_is_exit_six_and_preserves_database() {
        let fixture = fixture();
        let before = file_hash(&fixture.database_path);
        let cli = cli(&fixture.database_path, true, true);

        let (result, _) = invoke(
            &cli,
            &arguments(true),
            &FixedFreeSpaceProvider(u64::MAX),
            RuntimeContext::piped(),
        );

        let error = result.expect_err("incremental vacuum should be unavailable");
        assert_eq!(error.exit_code(), 6);
        assert_eq!(file_hash(&fixture.database_path), before);
    }

    #[cfg(unix)]
    #[test]
    fn sigint_during_full_vacuum_is_exit_eight_and_preserves_database() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 200,
            messages_per_session: 2,
            parts_per_message: 2,
            blob_size_per_part: 64 * 1_024,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let before_hash = file_hash(&fixture.database_path);
        let before_rows = table_counts(&fixture.database_path);
        let database_path = fixture.database_path.clone();
        let signals = signal::SignalController::new();
        signals.install().expect("signal handler should install");
        let interrupter = thread::spawn(move || {
            let parent = database_path
                .parent()
                .expect("fixture should have a parent");
            let database_name = database_path
                .file_name()
                .expect("fixture should have a file name")
                .to_string_lossy();
            let temporary_prefix = format!("{database_name}.oc-clean-tmp-");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let temporary_exists = fs::read_dir(parent)
                    .expect("fixture directory should be readable")
                    .filter_map(Result::ok)
                    .any(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(&temporary_prefix)
                    });
                if temporary_exists {
                    let status = Command::new("kill")
                        .args(["-s", "INT", &std::process::id().to_string()])
                        .status()
                        .expect("kill should send SIGINT");
                    assert!(status.success());
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "VACUUM temporary file should appear"
                );
                thread::sleep(Duration::from_millis(1));
            }
        });
        let cli = cli(&fixture.database_path, true, true);
        let mut input = Cursor::new(b"yes\n");
        let mut output = Vec::new();

        let result = run_with(
            &cli,
            &arguments(false),
            &mut input,
            &mut output,
            &FixedFreeSpaceProvider(u64::MAX),
            &NotHeldInspector,
            RuntimeContext::piped(),
            &signals,
        );

        interrupter.join().expect("interrupter should join");
        let error = result.expect_err("SIGINT should interrupt vacuum");
        assert_eq!(error.exit_code(), 8);
        assert_eq!(file_hash(&fixture.database_path), before_hash);
        assert_eq!(table_counts(&fixture.database_path), before_rows);
    }

    #[cfg(unix)]
    #[test]
    fn sigint_during_incremental_vacuum_is_exit_eight_and_preserves_database() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 200,
            messages_per_session: 2,
            parts_per_message: 2,
            blob_size_per_part: 64 * 1_024,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        prepare_incremental_bloat(&fixture);
        let before_rows = table_counts(&fixture.database_path);
        let before_size = fs::metadata(&fixture.database_path)
            .expect("fixture metadata should exist")
            .len();
        let database_path = fixture.database_path.clone();
        let signals = SignalController::new();
        signals.install().expect("signal handler should install");
        let interrupter = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let current_size = fs::metadata(&database_path)
                    .expect("fixture metadata should remain readable")
                    .len();
                if current_size < before_size {
                    let status = Command::new("kill")
                        .args(["-s", "INT", &std::process::id().to_string()])
                        .status()
                        .expect("kill should send SIGINT");
                    assert!(status.success());
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "incremental vacuum should reclaim at least one page"
                );
                thread::sleep(Duration::from_millis(1));
            }
        });
        let cli = cli(&fixture.database_path, true, true);
        let mut input = Cursor::new(b"yes\n");
        let mut output = Vec::new();

        let result = run_with(
            &cli,
            &arguments(true),
            &mut input,
            &mut output,
            &FixedFreeSpaceProvider(u64::MAX),
            &NotHeldInspector,
            RuntimeContext::piped(),
            &signals,
        );

        interrupter.join().expect("interrupter should join");
        let error = result.expect_err("SIGINT should interrupt incremental vacuum");
        assert_eq!(error.exit_code(), 8);
        assert_eq!(table_counts(&fixture.database_path), before_rows);
        let integrity: String = fixture
            .connect()
            .expect("fixture should reconnect")
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .expect("integrity check should run");
        assert_eq!(integrity, "ok");
    }

    #[test]
    fn applied_default_vacuum_shrinks_file_and_preserves_every_row() {
        let fixture = fixture();
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "DELETE FROM session WHERE id = ?1",
                [&fixture.session_ids[0]],
            )
            .expect("fixture session should delete");
        drop(connection);
        let before_size = fs::metadata(&fixture.database_path)
            .expect("fixture metadata should exist")
            .len();
        let before_rows = table_counts(&fixture.database_path);
        let cli = cli(&fixture.database_path, true, true);
        let (result, _) = invoke(
            &cli,
            &arguments(false),
            &FixedFreeSpaceProvider(u64::MAX),
            RuntimeContext::piped(),
        );

        result.expect("default vacuum should succeed");
        let after_size = fs::metadata(&fixture.database_path)
            .expect("fixture metadata should exist")
            .len();
        assert!(after_size < before_size);
        assert_eq!(table_counts(&fixture.database_path), before_rows);
    }
}
