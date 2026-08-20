use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::process::Command;

use super::{json, run};
use crate::fixture::{Fixture, FixtureConfig};

#[test]
fn seconds_epoch_timestamp_is_reported_and_fails_health_check() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    fixture
        .connect()
        .unwrap()
        .execute("UPDATE session SET time_updated = 1800000000", [])
        .unwrap();

    let output = run(&fixture, &["--json"]);

    assert_eq!(output.status.code(), Some(7));
    let report = json(&output);
    assert_eq!(report["timestamp_sanity"]["ok"], false);
    assert_eq!(
        report["timestamp_sanity"]["classification"],
        "likely-second-epoch"
    );
    assert!(
        report["timestamp_sanity"]["detail"]
            .as_str()
            .unwrap()
            .contains("seconds")
    );
}

#[test]
fn foreign_key_violation_is_reported_before_exit_code_seven() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    let connection = fixture.connect().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE doctor_fk (
                id TEXT PRIMARY KEY,
                session_id TEXT REFERENCES session(id)
             );
             PRAGMA foreign_keys = OFF;
             INSERT INTO doctor_fk VALUES ('broken', 'ses_missing');",
        )
        .unwrap();
    drop(connection);

    let output = run(&fixture, &["--json"]);

    assert_eq!(output.status.code(), Some(7));
    let report = json(&output);
    assert_eq!(report["foreign_key_check"]["ok"], false);
    assert!(
        report["foreign_key_check"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["table"] == "doctor_fk")
    );
}

#[test]
fn corrupted_database_page_uses_integrity_failure_exit_code() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 8,
        messages_per_session: 3,
        parts_per_message: 2,
        blob_size_per_part: 4_096,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    corrupt_btree_page(&fixture);

    let output = Command::new(env!("CARGO_BIN_EXE_oc-clean"))
        .args(["doctor", "--db"])
        .arg(&fixture.database_path)
        .output()
        .expect("oc-clean doctor should run");

    assert_eq!(output.status.code(), Some(7));
    assert!(String::from_utf8_lossy(&output.stderr).contains("integrity_check"));
}

fn corrupt_btree_page(fixture: &Fixture) {
    let connection = fixture.connect().unwrap();
    let page_size = u64::try_from(
        connection
            .pragma_query_value(None, "page_size", |row| row.get::<_, i64>(0))
            .unwrap(),
    )
    .unwrap();
    let page_number = u64::try_from(
        connection
            .query_row(
                "SELECT pageno FROM dbstat
                 WHERE name = 'part' AND pagetype IN ('internal', 'leaf') AND pageno > 1
                 LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
    )
    .unwrap();
    drop(connection);

    let page_start = (page_number - 1) * page_size;
    let mut file = OpenOptions::new()
        .write(true)
        .open(&fixture.database_path)
        .unwrap();
    file.seek(SeekFrom::Start(page_start)).unwrap();
    file.write_all(&[0xFF; 8]).unwrap();
    file.sync_all().unwrap();
}
