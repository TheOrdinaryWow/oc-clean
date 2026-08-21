use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rusqlite::Connection;

use super::fixture::{Fixture, FixtureConfig};
use super::support::{
    assert_code, binary, command, file_hash, insert_foreign_key_violation,
    make_partial_success_fixture, row_count,
};

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
