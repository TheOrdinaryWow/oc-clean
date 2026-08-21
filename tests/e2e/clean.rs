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
        .args(["--keep-recent", "3", "--dangerously-skip-confirm"])
        .output()
        .expect("invalid clean should run");

    assert_code(&output, 2);
    assert_eq!(file_hash(&fixture.database_path), before_hash);
    assert_eq!(session_ids(&fixture.database_path), before_ids);
}

#[test]
fn deletion_without_confirmation_refuses_and_leaves_the_database_untouched() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 5,
        archived_session_count: 3,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let before_hash = file_hash(&fixture.database_path);

    // A pipe is not a terminal, so the confirmation cannot be answered and must refuse.
    let refused = command(&fixture, "clean")
        .args(["--archived", "--no-vacuum"])
        .output()
        .expect("unconfirmed clean should run");

    assert_code(&refused, 2);
    assert_eq!(file_hash(&fixture.database_path), before_hash);
    assert_eq!(row_count(&fixture.database_path, "session"), 5);

    let previewed = command(&fixture, "clean")
        .args(["--archived", "--no-vacuum", "--dry-run"])
        .output()
        .expect("previewed clean should run");

    assert_code(&previewed, 0);
    assert_eq!(file_hash(&fixture.database_path), before_hash);

    let confirmed = command(&fixture, "clean")
        .args(["--archived", "--no-vacuum", "--dangerously-skip-confirm"])
        .output()
        .expect("confirmed clean should run");

    assert_code(&confirmed, 0);
    assert_eq!(row_count(&fixture.database_path, "session"), 2);
}

#[test]
fn exclude_selects_the_complement_of_include_and_the_two_cannot_be_combined() {
    let fixture = Fixture::build(&FixtureConfig {
        project_count: 3,
        session_count: 9,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");

    let included = command(&fixture, "clean")
        .args([
            "--include",
            "/fixture/project-0",
            "--no-vacuum",
            "--json",
            "--dry-run",
        ])
        .output()
        .expect("include dry-run should run");
    let excluded = command(&fixture, "clean")
        .args([
            "--exclude",
            "/fixture/project-0",
            "--no-vacuum",
            "--json",
            "--dry-run",
        ])
        .output()
        .expect("exclude dry-run should run");

    assert_code(&included, 0);
    assert_code(&excluded, 0);
    let included_sessions = json(&included)["impact"]["total_sessions"]
        .as_u64()
        .expect("include count should be numeric");
    let excluded_sessions = json(&excluded)["impact"]["total_sessions"]
        .as_u64()
        .expect("exclude count should be numeric");
    assert!(included_sessions > 0, "include should select something");
    assert_eq!(included_sessions + excluded_sessions, 9);

    // A glob reaches the same projects as the literal path it expands to.
    let globbed = command(&fixture, "clean")
        .args([
            "--include",
            "/fixture/project-[0]",
            "--no-vacuum",
            "--json",
            "--dry-run",
        ])
        .output()
        .expect("glob dry-run should run");
    assert_code(&globbed, 0);
    assert_eq!(
        json(&globbed)["impact"]["total_sessions"]
            .as_u64()
            .expect("glob count should be numeric"),
        included_sessions
    );

    let conflict = command(&fixture, "clean")
        .args(["--include", "/a", "--exclude", "/b", "--dry-run"])
        .output()
        .expect("conflicting selectors should run");
    assert_code(&conflict, 2);
}

#[test]
fn a_majority_selection_requires_a_second_confirmation() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 4,
        archived_session_count: 4,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let before_hash = file_hash(&fixture.database_path);

    let preview = command(&fixture, "clean")
        .args(["--archived", "--no-vacuum", "--json", "--dry-run"])
        .output()
        .expect("majority preview should run");

    assert_code(&preview, 0);
    let report = json(&preview);
    let selected = report["impact"]["total_sessions"]
        .as_u64()
        .expect("selection should be numeric");
    let total = report["impact"]["database_sessions"]
        .as_u64()
        .expect("database total should be numeric");
    assert!(
        selected * 2 >= total,
        "the fixture should select a majority: {selected} of {total}"
    );
    assert_eq!(file_hash(&fixture.database_path), before_hash);
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
        .arg("--dangerously-skip-confirm")
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
