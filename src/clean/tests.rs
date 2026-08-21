use std::cell::RefCell;
use std::io::Cursor;
use std::path::Path;

use crate::cli::{CleanArgs, Cli, Commands, LogFormat};
use crate::reclaim::headroom::FreeSpaceProvider;
use crate::safety::holders::{Completeness, HolderInspector, Inspection, Verdict};

use super::command::{RuntimeContext, run_with};
use super::signal::SignalController;
use super::{PhaseId, PhaseObserver};

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

#[derive(Default)]
struct Recorder(RefCell<Vec<PhaseId>>);

impl PhaseObserver for Recorder {
    fn entered(&self, phase: PhaseId) {
        self.0.borrow_mut().push(phase);
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
        json: false,
        log_format: LogFormat::Text,
    }
}

fn cli(path: &Path, incremental: bool) -> Cli {
    Cli {
        command: Commands::Clean(arguments(incremental)),
        db: Some(path.to_owned()),
        apply: true,
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
    assert!(result.is_ok() || result.is_err_and(|error| error.exit_code() == 10));
    recorder.0.into_inner()
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
fn p9_allows_clean_when_selection_makes_projected_rebuild_fit() {
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
    cli.apply = false;
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

    assert!(result.is_ok(), "projected rebuild should fit: {result:?}");
    assert!(recorder.0.into_inner().contains(&PhaseId::P10));
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
    let phases = recorder.0.into_inner();
    assert!(phases.contains(&PhaseId::P20));
    assert_eq!(phases.last(), Some(&PhaseId::P21));
    assert!(fixture.database_path.is_file());
}
