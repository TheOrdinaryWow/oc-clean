use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(all(target_os = "linux", feature = "bench-large"))]
use std::time::{Duration, Instant};

use crate::cli::{CleanArgs, Cli, Commands, LogMode};
use crate::error::Error;
use crate::paths::Target;
use crate::reclaim::headroom::FreeSpaceProvider;
use crate::safety::holders::{Completeness, HolderInfo, HolderInspector, Inspection, Verdict};
use clap::Parser;
use rusqlite::Connection;

use super::command::{
    RuntimeContext, inspect_read_only_phases, inspect_read_only_phases_sequential, run_with,
    run_with_git_path,
};
use super::signal::SignalController;
use super::{PhaseId, PhaseObserver, PhaseOperation};

#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

use fixture::{Fixture, FixtureConfig};

struct NotHeldInspector;

impl HolderInspector for NotHeldInspector {
    fn inspect(&self, _database_path: &Path) -> Inspection {
        Inspection {
            verdict: Verdict::NotHeld,
            completeness: Completeness::CompleteForVisibleProcesses,
        }
    }
}

struct HeldInspector;

impl HolderInspector for HeldInspector {
    fn inspect(&self, database_path: &Path) -> Inspection {
        Inspection {
            verdict: Verdict::Held(vec![HolderInfo {
                pid: 42,
                process_name: Some("fixture-holder".to_owned()),
                matched_paths: vec![database_path.to_owned()],
            }]),
            completeness: Completeness::CompleteForVisibleProcesses,
        }
    }
}

struct UnlimitedSpace;

impl FreeSpaceProvider for UnlimitedSpace {
    fn available_space(&self, _directory: &Path) -> std::io::Result<u64> {
        Ok(u64::MAX)
    }
}

struct FixedSpace(u64);

impl FreeSpaceProvider for FixedSpace {
    fn available_space(&self, _directory: &Path) -> std::io::Result<u64> {
        Ok(self.0)
    }
}

struct StorageMutationObserver {
    path: PathBuf,
    bytes: u64,
}

impl PhaseObserver for StorageMutationObserver {
    fn entered(&self, phase: PhaseId) {
        if phase == PhaseId::P9 {
            fs::OpenOptions::new()
                .write(true)
                .open(&self.path)
                .and_then(|file| file.set_len(self.bytes))
                .expect("storage mutation at P9 should succeed");
        }
    }
}

#[derive(Default)]
struct Recorder {
    phases: RefCell<Vec<PhaseId>>,
    operations: RefCell<Vec<(PhaseId, PhaseOperation)>>,
}

#[derive(Default)]
struct ThreadSafeRecorder {
    phases: Mutex<Vec<PhaseId>>,
}

impl PhaseObserver for ThreadSafeRecorder {
    fn entered(&self, phase: PhaseId) {
        self.phases
            .lock()
            .expect("phase recorder should not be poisoned")
            .push(phase);
    }
}

impl PhaseObserver for Recorder {
    fn entered(&self, phase: PhaseId) {
        self.phases.borrow_mut().push(phase);
    }

    fn performing(&self, phase: PhaseId, operation: PhaseOperation) {
        self.operations.borrow_mut().push((phase, operation));
    }
}

fn arguments(incremental: bool) -> CleanArgs {
    CleanArgs {
        older_than: None,
        project: None,
        larger_than: None,
        archived: true,
        orphans: true,
        keep_recent: 0,
        incremental,
        no_vacuum: false,
        gc_snapshots: true,
        prune_empty_projects: false,
        top: 10,
        json: false,
    }
}

fn cli(path: &Path, incremental: bool) -> Cli {
    Cli {
        command: Commands::Clean(arguments(incremental)),
        db: Some(path.to_owned()),
        channel: None,
        log: LogMode::Off,
        dry_run: false,
        force: false,
        force_schema: false,
        dangerously_skip_confirm: true,
        skip_backup: true,
    }
}

fn fixture(incremental: bool) -> Fixture {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 2,
        archived_session_count: 2,
        orphan_event_count: 1,
        dangling_parent_session_count: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    if incremental {
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .pragma_update(None, "auto_vacuum", 2)
            .expect("auto-vacuum should configure");
        connection
            .execute_batch("VACUUM")
            .expect("vacuum should apply");
    }
    fixture
}

fn recorded_phases(incremental: bool) -> Vec<PhaseId> {
    let fixture = fixture(incremental);
    let cli = cli(&fixture.database_path, incremental);
    let arguments = arguments(incremental);
    let recorder = Recorder::default();
    let signals = SignalController::new();
    let result = run_with(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
        &UnlimitedSpace,
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &signals,
        &recorder,
    );
    assert!(
        result.is_ok() || result.as_ref().is_err_and(|error| error.exit_code() == 10),
        "clean pipeline should complete or report partial success: {result:?}"
    );
    recorder.phases.into_inner()
}

#[test]
fn exact_phase_order_covers_default_and_conditional_paths() {
    assert_eq!(
        recorded_phases(false),
        vec![
            PhaseId::P1,
            PhaseId::P2,
            PhaseId::P3,
            PhaseId::P4,
            PhaseId::P5,
            PhaseId::P6,
            PhaseId::P7,
            PhaseId::P8,
            PhaseId::P9,
            PhaseId::P10,
            PhaseId::P11,
            PhaseId::P12,
            PhaseId::P13,
            PhaseId::P14,
            PhaseId::P15,
            PhaseId::P16,
            PhaseId::P17,
            PhaseId::P17b,
            PhaseId::P18,
            PhaseId::P19,
            PhaseId::P20,
            PhaseId::P21,
        ]
    );
    assert_eq!(
        recorded_phases(true),
        vec![
            PhaseId::P1,
            PhaseId::P2,
            PhaseId::P3,
            PhaseId::P3b,
            PhaseId::P4,
            PhaseId::P5,
            PhaseId::P6,
            PhaseId::P7,
            PhaseId::P8,
            PhaseId::P9,
            PhaseId::P10,
            PhaseId::P11,
            PhaseId::P12,
            PhaseId::P13,
            PhaseId::P14,
            PhaseId::P15,
            PhaseId::P16,
            PhaseId::P17,
            PhaseId::P17b,
            PhaseId::P18,
            PhaseId::P20,
            PhaseId::P21,
        ]
    );
}

#[test]
fn p9_precedes_impact_summary_work() {
    const MUTATED_STORAGE_BYTES: u64 = 16 * 1_024 * 1_024;

    let fixture = fixture(false);
    let mut cli = cli(&fixture.database_path, false);
    cli.dry_run = true;
    let mut arguments = arguments(false);
    arguments.json = true;
    let storage_path = fixture
        .storage_dir
        .join("session")
        .join(format!("{}.json", fixture.session_ids[0]));
    let observer = StorageMutationObserver {
        path: storage_path,
        bytes: MUTATED_STORAGE_BYTES,
    };
    let mut output = Vec::new();

    run_with(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut output,
        &UnlimitedSpace,
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &SignalController::new(),
        &observer,
    )
    .expect("dry-run should succeed");

    let report: serde_json::Value =
        serde_json::from_slice(&output).expect("dry-run output should be JSON");
    assert!(
        report["impact"]["total_bytes"]
            .as_u64()
            .expect("total bytes should be numeric")
            >= MUTATED_STORAGE_BYTES
    );
}

#[test]
fn selection_operations_are_attributed_to_the_normative_phases() {
    let fixture = fixture(false);
    let cli = cli(&fixture.database_path, false);
    let arguments = arguments(false);
    let recorder = Recorder::default();
    let result = run_with(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
        &UnlimitedSpace,
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &SignalController::new(),
        &recorder,
    );
    assert!(
        result.is_ok() || result.as_ref().is_err_and(|error| error.exit_code() == 10),
        "clean pipeline should complete or report partial success: {result:?}"
    );
    assert_eq!(
        recorder.operations.into_inner(),
        vec![
            (PhaseId::P6, PhaseOperation::Retention),
            (PhaseId::P7, PhaseOperation::PredicateSelection),
            (PhaseId::P8, PhaseOperation::DescendantExpansion),
            (PhaseId::P13, PhaseOperation::OrphanSelection),
        ]
    );
}

#[test]
fn read_only_phase_failures_surface_in_declaration_order() {
    let fixture = fixture(false);
    let mut cli = cli(&fixture.database_path, false);
    cli.dry_run = false;
    let mut arguments = arguments(false);
    arguments.archived = false;
    arguments.orphans = false;

    for _ in 0..20 {
        let error = run_with(
            &cli,
            &arguments,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
            &UnlimitedSpace,
            &HeldInspector,
            RuntimeContext {
                stdin_is_terminal: false,
                stdout_is_terminal: false,
            },
            &SignalController::new(),
            &ThreadSafeRecorder::default(),
        )
        .expect_err("holder and selector phases should both fail");

        assert!(matches!(error, Error::DatabaseBusy { .. }));
        assert_eq!(error.exit_code(), 5);
    }
}

#[test]
fn independent_read_only_phases_are_entered_before_results_are_joined() {
    let fixture = fixture(false);
    let mut cli = cli(&fixture.database_path, false);
    cli.dry_run = true;
    let mut arguments = arguments(false);
    arguments.archived = false;
    arguments.orphans = false;
    let recorder = ThreadSafeRecorder::default();

    let _ = run_with(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
        &UnlimitedSpace,
        &HeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &SignalController::new(),
        &recorder,
    );

    assert_eq!(
        *recorder
            .phases
            .lock()
            .expect("phase recorder should not be poisoned"),
        vec![PhaseId::P1, PhaseId::P2, PhaseId::P3, PhaseId::P4]
    );
}

#[test]
fn dry_run_and_apply_report_identical_selection_impact() {
    fn report(fixture: &Fixture, apply: bool) -> serde_json::Value {
        let mut cli = cli(&fixture.database_path, false);
        cli.dry_run = !apply;
        let mut arguments = arguments(false);
        arguments.orphans = false;
        arguments.no_vacuum = true;
        arguments.gc_snapshots = false;
        arguments.json = true;
        let mut output = Vec::new();

        run_with(
            &cli,
            &arguments,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut output,
            &UnlimitedSpace,
            &NotHeldInspector,
            RuntimeContext {
                stdin_is_terminal: false,
                stdout_is_terminal: false,
            },
            &SignalController::new(),
            &ThreadSafeRecorder::default(),
        )
        .expect("clean should succeed");

        serde_json::from_slice(&output).expect("clean output should be JSON")
    }

    let dry_run = report(&fixture(false), false);
    let applied = report(&fixture(false), true);

    assert_eq!(dry_run["impact"], applied["impact"]);
    assert_eq!(dry_run["impact"]["total_sessions"], 2);
    assert_eq!(applied["deleted_sessions"], 2);
}

#[test]
fn parallel_read_only_phases_match_the_sequential_reference() {
    let fixture = fixture(false);
    let target = Target::File(fixture.database_path.clone());
    let cli = cli(&fixture.database_path, false);
    let arguments = arguments(false);

    let sequential = inspect_read_only_phases_sequential(
        &target,
        &fixture.database_path,
        &cli,
        &arguments,
        &NotHeldInspector,
    )
    .expect("sequential read-only phases should succeed");
    let parallel = inspect_read_only_phases(
        &target,
        &fixture.database_path,
        &cli,
        &arguments,
        &NotHeldInspector,
    )
    .expect("parallel read-only phases should succeed");

    assert_eq!(parallel, sequential);
}

#[test]
#[cfg(all(target_os = "linux", feature = "bench-large"))]
#[ignore = "performance characterization on a large fixture"]
fn read_only_phase_parallel_benchmark_large_fixture() {
    let fixture = Fixture::build(&FixtureConfig {
        project_count: 100,
        session_count: 5_000,
        archived_session_count: 5_000,
        messages_per_session: 2,
        parts_per_message: 2,
        blob_size_per_part: 1_024,
        ..FixtureConfig::default()
    })
    .expect("large fixture should build");
    let target = Target::File(fixture.database_path.clone());
    let cli = cli(&fixture.database_path, false);
    let arguments = arguments(false);
    let holder_inspector = crate::safety::holders::linux::LinuxHolderInspector::default();
    let mut sequential_times = Vec::with_capacity(9);
    let mut parallel_times = Vec::with_capacity(9);

    for iteration in 0..9 {
        let measure_sequential = || {
            let started = Instant::now();
            inspect_read_only_phases_sequential(
                &target,
                &fixture.database_path,
                &cli,
                &arguments,
                &holder_inspector,
            )
            .expect("sequential read-only phases should succeed");
            started.elapsed()
        };
        let measure_parallel = || {
            let started = Instant::now();
            inspect_read_only_phases(
                &target,
                &fixture.database_path,
                &cli,
                &arguments,
                &holder_inspector,
            )
            .expect("parallel read-only phases should succeed");
            started.elapsed()
        };

        if iteration % 2 == 0 {
            sequential_times.push(measure_sequential());
            parallel_times.push(measure_parallel());
        } else {
            parallel_times.push(measure_parallel());
            sequential_times.push(measure_sequential());
        }
    }

    sequential_times.sort_unstable();
    parallel_times.sort_unstable();
    let sequential = sequential_times[sequential_times.len() / 2];
    let parallel = parallel_times[parallel_times.len() / 2];
    let speedup = sequential.as_secs_f64() / parallel.as_secs_f64();
    println!(
        "fixture_sessions=10000 fixture_parts=40000 sequential_ms={:.3} parallel_ms={:.3} speedup={speedup:.3}x",
        duration_ms(sequential),
        duration_ms(parallel),
    );
}

#[cfg(all(target_os = "linux", feature = "bench-large"))]
fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[test]
fn orphan_event_aggregate_does_not_inflate_deleted_session_count() {
    let fixture = Fixture::build(&FixtureConfig {
        project_count: 5,
        session_count: 10,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let connection = fixture.connect().expect("fixture should connect");
    connection
        .execute("UPDATE session SET time_created = 0, time_updated = 0", [])
        .expect("sessions should be older than the selector cutoff");
    connection
        .execute(
            "INSERT INTO event_sequence VALUES ('ses_OrphanAggregate', 1, NULL)",
            [],
        )
        .expect("orphan event sequence should insert");
    connection
        .execute(
            "INSERT INTO event VALUES ('event-orphan-aggregate', 'ses_OrphanAggregate', 1, 'session.orphan', '{}')",
            [],
        )
        .expect("orphan event should insert");
    drop(connection);

    let mut arguments = arguments(false);
    arguments.archived = false;
    arguments.older_than = Some("1d".parse().expect("duration should parse"));
    arguments.keep_recent = 1;
    arguments.no_vacuum = true;
    arguments.gc_snapshots = false;
    arguments.json = true;
    let cli = cli(&fixture.database_path, false);
    let mut output = Vec::new();
    run_with(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut output,
        &UnlimitedSpace,
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &SignalController::new(),
        &Recorder::default(),
    )
    .expect("combined session and orphan cleanup should succeed");

    let report: serde_json::Value =
        serde_json::from_slice(&output).expect("final output should be JSON");
    assert_eq!(report["deleted_sessions"], 5);
    let remaining: i64 = fixture
        .connect()
        .expect("fixture should reconnect")
        .query_row("SELECT COUNT(*) FROM session", [], |row| row.get(0))
        .expect("remaining sessions should count");
    assert_eq!(remaining, 5);
}

#[test]
fn missing_git_during_snapshot_gc_keeps_command_successful() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 1,
        archived_session_count: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let arguments = CleanArgs {
        orphans: false,
        no_vacuum: true,
        gc_snapshots: true,
        ..arguments(false)
    };
    let cli = cli(&fixture.database_path, false);
    let result = run_with_git_path(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
        &UnlimitedSpace,
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &SignalController::new(),
        &Recorder::default(),
        Some(OsStr::new("oc-clean-nonexistent-git")),
    );
    assert!(
        result.is_ok(),
        "missing git should be an informational skip: {result:?}"
    );
}

fn run_project_pruning(prune_empty_projects: bool) -> (Fixture, Vec<String>) {
    let fixture = Fixture::build(&FixtureConfig {
        project_count: 2,
        session_count: 1,
        archived_session_count: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let mut argv = vec![
        OsString::from("oc-clean"),
        OsString::from("--db"),
        fixture.database_path.as_os_str().to_owned(),
        OsString::from("--dangerously-skip-confirm"),
        OsString::from("clean"),
        OsString::from("--archived"),
        OsString::from("--keep-recent"),
        OsString::from("0"),
        OsString::from("--no-vacuum"),
    ];
    if prune_empty_projects {
        argv.push(OsString::from("--prune-empty-projects"));
    }
    let cli = Cli::try_parse_from(argv).expect("clean arguments should parse");
    let Commands::Clean(arguments) = &cli.command else {
        panic!("clean command should parse");
    };
    run_with(
        &cli,
        arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
        &UnlimitedSpace,
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &SignalController::new(),
        &Recorder::default(),
    )
    .expect("applied clean should succeed");
    let connection = fixture.connect().expect("fixture should reconnect");
    let projects = connection
        .prepare("SELECT id FROM project ORDER BY id")
        .expect("project query should prepare")
        .query_map([], |row| row.get(0))
        .expect("project query should execute")
        .collect::<Result<Vec<String>, _>>()
        .expect("project ids should decode");
    drop(connection);
    (fixture, projects)
}

#[test]
fn prune_empty_projects_extends_pruning_to_preexisting_empty_projects() {
    let (default_fixture, default_projects) = run_project_pruning(false);
    assert_eq!(default_projects, ["project-1"]);
    assert!(!default_fixture.snapshot_dir.join("project-0").exists());
    assert!(default_fixture.snapshot_dir.join("project-1").is_dir());

    let (explicit_fixture, explicit_projects) = run_project_pruning(true);
    assert_eq!(explicit_projects, Vec::<String>::new());
    assert!(!explicit_fixture.snapshot_dir.join("project-0").exists());
    assert!(!explicit_fixture.snapshot_dir.join("project-1").exists());
}

fn orphan_snapshot_survives_cleanup(orphans: bool) -> bool {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 1,
        archived_session_count: 1,
        orphan_snapshot_dir_count: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let orphan = fixture
        .orphan_snapshot_dirs
        .first()
        .expect("orphan snapshot should exist")
        .clone();
    let arguments = CleanArgs {
        orphans,
        gc_snapshots: false,
        no_vacuum: true,
        ..arguments(false)
    };
    let cli = cli(&fixture.database_path, false);
    run_with(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
        &UnlimitedSpace,
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &SignalController::new(),
        &Recorder::default(),
    )
    .expect("applied clean should succeed");
    assert!(Connection::open(&fixture.database_path).is_ok());
    orphan.is_dir()
}

#[test]
fn preexisting_orphan_snapshots_require_the_orphans_selector() {
    assert!(orphan_snapshot_survives_cleanup(false));
    assert!(!orphan_snapshot_survives_cleanup(true));
}

#[test]
fn p9_refuses_when_the_full_delete_batch_wal_exceeds_available_space() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 600,
        archived_session_count: 600,
        messages_per_session: 1,
        parts_per_message: 1,
        blob_size_per_part: 4 * 1_024,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let target = crate::paths::Target::File(fixture.database_path.clone());
    let database = crate::db::open_read_only(&target, crate::db::ConnectionOptions::default())
        .expect("fixture should open read-only");
    let current_live_bytes = crate::analyze::space::analyze(&database)
        .expect("fixture space should analyze")
        .file
        .live_bytes;
    drop(database);

    let mut cli = cli(&fixture.database_path, false);
    cli.dry_run = true;
    let mut arguments = arguments(false);
    arguments.json = true;
    let signals = SignalController::new();
    let mut baseline_output = Vec::new();
    run_with(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut baseline_output,
        &UnlimitedSpace,
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &signals,
        &Recorder::default(),
    )
    .expect("baseline dry-run should succeed");
    let baseline: serde_json::Value =
        serde_json::from_slice(&baseline_output).expect("dry-run output should be JSON");
    let projected_live_bytes = baseline["impact"]["estimated_post_vacuum_bytes"]
        .as_u64()
        .expect("projected live bytes should be numeric");
    let available_bytes = current_live_bytes.saturating_sub(1);
    assert!(available_bytes < current_live_bytes);
    assert!(available_bytes > projected_live_bytes);

    let recorder = Recorder::default();
    let result = run_with(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
        &FixedSpace(available_bytes),
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &signals,
        &recorder,
    );

    let error = result.expect_err("full delete-batch WAL should exceed available space");
    assert!(matches!(
        error,
        crate::error::Error::InsufficientDiskSpace { .. }
    ));
    let phases = recorder.phases.into_inner();
    assert!(phases.contains(&PhaseId::P9));
    assert!(!phases.contains(&PhaseId::P10));
}

#[test]
fn filesystem_cleanup_failure_continues_through_swap_and_exits_partial_success() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 1,
        archived_session_count: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let connection = fixture.connect().expect("fixture should connect");
    connection
        .pragma_update(None, "foreign_keys", false)
        .expect("foreign keys should disable for malformed fixture setup");
    connection
        .execute_batch(
            "UPDATE session SET project_id = '../unsafe' WHERE project_id = 'project-0';
             UPDATE project_directory SET project_id = '../unsafe' WHERE project_id = 'project-0';
             UPDATE workspace SET project_id = '../unsafe' WHERE project_id = 'project-0';
             UPDATE permission SET project_id = '../unsafe' WHERE project_id = 'project-0';
             UPDATE project SET id = '../unsafe' WHERE id = 'project-0';",
        )
        .expect("project references should update");
    drop(connection);

    let arguments = CleanArgs {
        orphans: false,
        gc_snapshots: false,
        ..arguments(false)
    };
    let cli = cli(&fixture.database_path, false);
    let recorder = Recorder::default();
    let signals = SignalController::new();
    let error = run_with(
        &cli,
        &arguments,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
        &UnlimitedSpace,
        &NotHeldInspector,
        RuntimeContext {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
        },
        &signals,
        &recorder,
    )
    .expect_err("unsafe snapshot id should yield partial success");

    assert_eq!(error.exit_code(), 10);
    let phases = recorder.phases.into_inner();
    assert!(phases.contains(&PhaseId::P20));
    assert_eq!(phases.last(), Some(&PhaseId::P21));
    assert!(fixture.database_path.is_file());
}
