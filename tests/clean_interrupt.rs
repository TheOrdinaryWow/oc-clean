#![cfg(unix)]

#[allow(clippy::duplicate_mod, dead_code)]
#[path = "support/fixture.rs"]
mod fixture;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rusqlite::Connection;

use oc_clean::delete::sessions::DeleteOptions;

use fixture::{Fixture, FixtureConfig};

#[test]
fn sigint_after_a_committed_delete_batch_stops_safely() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 15_000,
        archived_session_count: 15_000,
        messages_per_session: 1,
        parts_per_message: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let initial_sessions = count_sessions(&fixture.database_path);
    let mut child = Command::new(env!("CARGO_BIN_EXE_oc-clean"))
        .args([
            "--db",
            fixture.database_path.to_str().expect("UTF-8 fixture path"),
            "--apply",
            "--dangerously-skip-confirm",
            "--skip-backup",
            "clean",
            "--archived",
            "--keep-recent",
            "0",
            "--no-vacuum",
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
    let signal_status = Command::new("kill")
        .args(["-s", "INT", &child.id().to_string()])
        .status()
        .expect("kill should send SIGINT");
    assert!(signal_status.success());

    let output = child.wait_with_output().expect("clean should exit");
    let stderr = stderr_reader.join().expect("stderr reader should join");
    assert_eq!(output.status.code(), Some(8), "stderr: {stderr}");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("stopped safely"), "stdout: {stdout}");

    let connection = Connection::open(&fixture.database_path).expect("fixture should reopen");
    let integrity = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .expect("integrity check should run");
    assert_eq!(integrity, "ok");
    let foreign_key_violations = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("foreign-key check should run");
    assert_eq!(foreign_key_violations, 0);
    let remaining = connection
        .query_row("SELECT COUNT(*) FROM session", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("remaining sessions should count");
    let deleted = initial_sessions - u64::try_from(remaining).expect("non-negative count");
    assert!(deleted > 0);
    assert_eq!(deleted % DeleteOptions::default().batch_size as u64, 0);
}

fn count_sessions(path: &std::path::Path) -> u64 {
    Connection::open(path)
        .expect("fixture should open")
        .query_row("SELECT COUNT(*) FROM session", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| u64::try_from(count).expect("non-negative count"))
        .expect("sessions should count")
}
