use std::fs;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Read};
#[cfg(target_arch = "x86_64")]
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Duration;

use rusqlite::Connection;

use super::fixture::{Fixture, FixtureConfig};
#[cfg(target_os = "linux")]
use super::holder::HoldingChild;
use super::support::{
    assert_code, binary, command, file_hash, insert_foreign_key_violation,
    make_partial_success_fixture, row_count,
};

#[cfg(target_os = "linux")]
#[test]
fn clean_with_real_holder_preserves_preview_and_refuses_apply() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    let before_hash = file_hash(&fixture.database_path);
    let before_sessions = row_count(&fixture.database_path, "session");
    let _holder = HoldingChild::spawn(&fixture.database_path);

    let preview = command(&fixture, "clean")
        .args(["--archived", "--keep-recent", "0", "--json"])
        .output()
        .expect("clean preview should run");

    assert_code(&preview, 0);
    assert_eq!(file_hash(&fixture.database_path), before_hash);
    assert_eq!(
        row_count(&fixture.database_path, "session"),
        before_sessions
    );

    let apply = command(&fixture, "clean")
        .args([
            "--archived",
            "--keep-recent",
            "0",
            "--apply",
            "--dangerously-skip-confirm",
            "--json",
        ])
        .output()
        .expect("clean apply should run");

    assert_code(&apply, 5);
    assert_eq!(file_hash(&fixture.database_path), before_hash);
    assert_eq!(
        row_count(&fixture.database_path, "session"),
        before_sessions
    );
}

#[cfg(target_arch = "x86_64")]
#[test]
fn unsupported_platform_binary_produces_exit_nine() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let injected_binary = directory.path().join("oc-clean-unsupported-platform");
    fs::copy(env!("CARGO_BIN_EXE_oc-clean"), &injected_binary)
        .expect("compiled binary should copy");
    inject_unsupported_platform(&injected_binary);

    let output = Command::new(injected_binary)
        .arg("analyze")
        .arg("--db")
        .arg(&fixture.database_path)
        .arg("--json")
        .env_remove("NO_COLOR")
        .env_remove("OCC_DB")
        .output()
        .expect("injected compiled binary should run");

    assert_code(&output, 9);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(std::env::consts::OS),
        "stderr should identify the unsupported target: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_arch = "x86_64")]
fn inject_unsupported_platform(path: &std::path::Path) {
    // Rust deduplicates the host OS constant and its first match literal. Flip the copied
    // binary's first equality branch so the unchanged production fallback receives the real OS.
    const FIRST_PLATFORM_MATCH: &[u8] = &[0xa8, 0x01, 0x75, 0x21, 0x48, 0x8d];
    let mut image = fs::read(path).expect("compiled binary should read");
    let positions = image
        .windows(FIRST_PLATFORM_MATCH.len())
        .enumerate()
        .filter_map(|(index, bytes)| (bytes == FIRST_PLATFORM_MATCH).then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(
        positions.len(),
        4,
        "compiled binary should contain one first-platform branch per command"
    );
    for position in positions {
        image[position + 2] = 0x74;
    }
    fs::write(path, image).expect("injected compiled binary should write");
}

#[test]
fn malformed_database_produces_exit_one() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    fs::write(
        &fixture.database_path,
        b"generated malformed sqlite fixture",
    )
    .expect("fixture should become malformed");

    let output = command(&fixture, "analyze")
        .arg("--json")
        .output()
        .expect("analyze should run");

    assert_code(&output, 1);
}

#[test]
fn missing_database_produces_exit_three() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let missing = directory.path().join("missing-opencode.db");

    let output = binary()
        .arg("analyze")
        .arg("--db")
        .arg(missing)
        .arg("--json")
        .output()
        .expect("analyze should run");

    assert_code(&output, 3);
}

#[cfg(unix)]
#[test]
fn symlink_cycle_produces_exit_one_for_doctor_and_vacuum() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let first = directory.path().join("cycle-a.db");
    let second = directory.path().join("cycle-b.db");
    std::os::unix::fs::symlink("cycle-b.db", &first).expect("first cycle symlink should create");
    std::os::unix::fs::symlink("cycle-a.db", &second).expect("second cycle symlink should create");

    for subcommand in ["doctor", "vacuum"] {
        let output = binary()
            .arg(subcommand)
            .arg("--db")
            .arg(&first)
            .output()
            .unwrap_or_else(|error| panic!("{subcommand} should run: {error}"));

        assert_code(&output, 1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("symlink cycle while resolving"),
            "{subcommand} should identify the symlink cycle: {stderr}"
        );
        assert!(
            stderr.contains(&first.display().to_string()),
            "{subcommand} should identify the database path: {stderr}"
        );
    }
}

#[test]
fn incompatible_schema_produces_exit_four_without_mutation() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    fixture
        .connect()
        .expect("fixture should connect")
        .execute_batch("DROP TABLE session")
        .expect("required table should drop");
    let before_hash = file_hash(&fixture.database_path);

    let output = command(&fixture, "clean")
        .args([
            "--archived",
            "--keep-recent",
            "0",
            "--apply",
            "--dangerously-skip-confirm",
        ])
        .output()
        .expect("clean should run");

    assert_code(&output, 4);
    assert_eq!(file_hash(&fixture.database_path), before_hash);
}

#[cfg(target_os = "linux")]
#[test]
fn held_database_produces_exit_five() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    let holder = fixture.connect().expect("fixture holder should connect");

    let output = command(&fixture, "vacuum")
        .args(["--apply", "--dangerously-skip-confirm"])
        .output()
        .expect("vacuum should run");

    assert_code(&output, 5);
    drop(holder);
}

#[test]
fn incremental_vacuum_lock_contention_produces_exit_five() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 12,
        messages_per_session: 2,
        parts_per_message: 2,
        blob_size_per_part: 64 * 1_024,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let holder = fixture.connect().expect("fixture holder should connect");
    holder
        .pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .expect("incremental auto-vacuum should be requested");
    holder
        .execute_batch("VACUUM; DELETE FROM session;")
        .expect("fixture should enter incremental mode and gain free pages");
    let freelist_pages = holder
        .pragma_query_value(None, "freelist_count", |row| row.get::<_, i64>(0))
        .expect("fixture freelist should be readable");
    assert!(freelist_pages > 0, "fixture should have reclaimable pages");
    holder
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("holder should acquire a competing write lock");

    let output = command(&fixture, "vacuum")
        .args([
            "--incremental",
            "--apply",
            "--dangerously-skip-confirm",
            "--force",
        ])
        .output()
        .expect("incremental vacuum should run");

    assert_code(&output, 5);
    holder
        .execute_batch("ROLLBACK;")
        .expect("holder transaction should roll back");
}

#[test]
fn clean_incremental_on_auto_vacuum_none_produces_exit_six() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 3,
        archived_session_count: 2,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let before_hash = file_hash(&fixture.database_path);

    let output = command(&fixture, "clean")
        .args([
            "--archived",
            "--keep-recent",
            "0",
            "--incremental",
            "--apply",
            "--dangerously-skip-confirm",
        ])
        .output()
        .expect("clean should run");

    assert_code(&output, 6);
    assert_eq!(file_hash(&fixture.database_path), before_hash);
    assert_eq!(row_count(&fixture.database_path, "session"), 3);
}

#[test]
fn foreign_key_failure_produces_exit_seven() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    insert_foreign_key_violation(&fixture);

    let output = command(&fixture, "doctor")
        .arg("--json")
        .output()
        .expect("doctor should run");

    assert_code(&output, 7);
}

#[cfg(unix)]
#[test]
fn interrupt_after_committed_batch_produces_exit_eight() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 15_000,
        archived_session_count: 15_000,
        messages_per_session: 1,
        parts_per_message: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let mut child = command(&fixture, "clean")
        .args([
            "--archived",
            "--keep-recent",
            "0",
            "--no-vacuum",
            "--apply",
            "--dangerously-skip-confirm",
            "--skip-backup",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("clean subprocess should spawn");
    let stderr = child.stderr.take().expect("stderr should be piped");
    let (committed_tx, committed_rx) = mpsc::channel();
    let stderr_reader = thread::spawn(move || {
        let mut captured = String::new();
        let mut notified = false;
        for line in BufReader::new(stderr).lines() {
            let line = line.expect("stderr should be readable");
            if !notified && line.contains("committed delete batch") {
                committed_tx.send(()).expect("test receiver should remain");
                notified = true;
            }
            captured.push_str(&line);
            captured.push('\n');
        }
        captured
    });
    committed_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("a delete batch should commit before timeout");
    let signal = std::process::Command::new("kill")
        .args(["-s", "INT", &child.id().to_string()])
        .status()
        .expect("kill should send SIGINT");
    assert!(signal.success());

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout should be piped")
        .read_to_string(&mut stdout)
        .expect("stdout should be readable");
    let status = child.wait().expect("clean should exit");
    let stderr = stderr_reader.join().expect("stderr reader should join");
    assert_eq!(status.code(), Some(8), "stdout: {stdout}\nstderr: {stderr}");
    assert!(row_count(&fixture.database_path, "session") < 15_000);
}

#[test]
fn filesystem_cleanup_failure_produces_exit_ten() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 1,
        archived_session_count: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    make_partial_success_fixture(&fixture);

    let output = command(&fixture, "clean")
        .args([
            "--archived",
            "--keep-recent",
            "0",
            "--no-vacuum",
            "--apply",
            "--dangerously-skip-confirm",
        ])
        .output()
        .expect("clean should run");

    assert_code(&output, 10);
    let integrity = Connection::open(&fixture.database_path)
        .expect("fixture should reopen")
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .expect("integrity check should run");
    assert_eq!(integrity, "ok");
}
