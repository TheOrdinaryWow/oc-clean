use std::collections::BTreeSet;

use super::fixture::{Fixture, FixtureConfig};
use super::support::{
    assert_code, command, file_hash, insert_dangling_session, json, max_session_time, row_count,
    session_ids,
};

#[test]
fn full_clean_pipeline_leaves_doctor_clean_and_zero_orphans() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 3,
        messages_per_session: 2,
        parts_per_message: 2,
        orphan_event_count: 2,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let oldest = max_session_time(&fixture) - 10_000;
    insert_dangling_session(&fixture, "ses_DanglingSweep", oldest);

    let clean = command(&fixture, "clean")
        .args([
            "--older-than",
            "1d",
            "--orphans",
            "--keep-recent",
            "1",
            "--json",
            "--apply",
            "--dangerously-skip-confirm",
        ])
        .output()
        .expect("full clean should run");
    assert_code(&clean, 0);

    let doctor = command(&fixture, "doctor")
        .arg("--json")
        .output()
        .expect("doctor should run after clean");

    assert_code(&doctor, 0);
    let report = json(&doctor);
    assert_eq!(report["integrity_check"]["ok"], true);
    assert_eq!(report["foreign_key_check"]["ok"], true);
    for class in [
        "orphan_events",
        "dangling_parent_sessions",
        "foreign_key_dangling_rows",
        "orphan_storage_files",
        "orphan_snapshot_directories",
    ] {
        assert_eq!(report["orphans"][class]["count"], 0, "orphan class {class}");
    }
}

#[test]
fn repeated_clean_is_idempotent() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 5,
        archived_session_count: 3,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");

    let first = command(&fixture, "clean")
        .args([
            "--archived",
            "--keep-recent",
            "0",
            "--no-vacuum",
            "--json",
            "--apply",
            "--dangerously-skip-confirm",
        ])
        .output()
        .expect("first clean should run");
    let second = command(&fixture, "clean")
        .args([
            "--archived",
            "--keep-recent",
            "0",
            "--no-vacuum",
            "--json",
            "--apply",
            "--dangerously-skip-confirm",
        ])
        .output()
        .expect("second clean should run");

    assert_code(&first, 0);
    assert_code(&second, 0);
    let first = json(&first);
    let second = json(&second);
    assert_eq!(first["deleted_sessions"], 3);
    assert_eq!(second["deleted_sessions"], 0);
    assert_eq!(second["deleted_rows"], 0);
    assert_eq!(second["impact"]["total_sessions"], 0);
    assert_eq!(second["impact"]["orphan_rows"], 0);
    assert_eq!(second["impact"]["storage_files"], 0);
    assert_eq!(second["impact"]["snapshot_directories"], 0);
}

#[test]
fn keep_recent_alone_exits_two_without_mutation() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 5,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let before_hash = file_hash(&fixture.database_path);
    let before_ids = session_ids(&fixture.database_path);

    let output = command(&fixture, "clean")
        .args([
            "--keep-recent",
            "3",
            "--apply",
            "--dangerously-skip-confirm",
        ])
        .output()
        .expect("invalid clean should run");

    assert_code(&output, 2);
    assert_eq!(file_hash(&fixture.database_path), before_hash);
    assert_eq!(session_ids(&fixture.database_path), before_ids);
}

#[test]
fn apply_without_any_predicate_exits_two_without_mutation() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 5,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let before_hash = file_hash(&fixture.database_path);

    let output = command(&fixture, "clean")
        .args(["--apply", "--dangerously-skip-confirm"])
        .output()
        .expect("invalid clean should run");

    assert_code(&output, 2);
    assert_eq!(file_hash(&fixture.database_path), before_hash);
    assert_eq!(row_count(&fixture.database_path, "session"), 5);
}

#[test]
fn keep_recent_protects_a_dangling_retention_root_from_orphan_sweep() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    let baseline = max_session_time(&fixture);
    insert_dangling_session(&fixture, "ses_DanglingRecent", baseline + 1_000);
    insert_dangling_session(&fixture, "ses_DanglingOld", baseline - 1_000);

    let output = command(&fixture, "clean")
        .args([
            "--orphans",
            "--keep-recent",
            "1",
            "--no-vacuum",
            "--json",
            "--apply",
            "--dangerously-skip-confirm",
        ])
        .output()
        .expect("orphan clean should run");

    assert_code(&output, 0);
    assert_eq!(
        session_ids(&fixture.database_path),
        BTreeSet::from(["ses_0".to_owned(), "ses_DanglingRecent".to_owned()])
    );
    assert_eq!(json(&output)["deleted_sessions"], 1);
}
