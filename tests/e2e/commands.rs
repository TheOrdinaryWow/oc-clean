use std::collections::BTreeSet;
use std::fs;

use super::fixture::{Fixture, FixtureConfig};
use super::support::{assert_code, command, file_hash, json, row_count, session_ids, table_counts};

#[test]
fn analyze_reports_every_layer_in_human_and_json_forms() {
    let fixture = Fixture::build(&FixtureConfig {
        project_count: 2,
        session_count: 6,
        messages_per_session: 2,
        parts_per_message: 2,
        blob_size_per_part: 1_024,
        orphan_event_count: 1,
        dangling_parent_session_count: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");

    let human = command(&fixture, "analyze")
        .output()
        .expect("human analyze should run");
    let machine = command(&fixture, "analyze")
        .arg("--json")
        .output()
        .expect("JSON analyze should run");

    assert_code(&human, 0);
    assert_code(&machine, 0);
    let human = String::from_utf8(human.stdout).expect("human report should be UTF-8");
    for heading in [
        "Database File Space",
        "Row Counts",
        "Table and Index Space",
        "Project Attribution",
        "Largest Sessions",
        "Orphan Census",
        "Age Distribution",
        "External Directories",
    ] {
        assert!(human.contains(heading), "missing human layer {heading}");
    }
    let report = json(&machine);
    assert_eq!(report["schema_version"], 1);
    for key in [
        "file_space",
        "row_counts",
        "table_space",
        "project_attribution",
        "largest_sessions",
        "orphans",
        "age_distribution",
        "external_directories",
    ] {
        assert!(report.get(key).is_some(), "missing JSON layer {key}");
    }
}

#[test]
fn doctor_reports_schema_integrity_orphans_holders_and_headroom() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 4,
        orphan_event_count: 1,
        dangling_parent_session_count: 1,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");

    let human = command(&fixture, "doctor")
        .output()
        .expect("human doctor should run");
    let machine = command(&fixture, "doctor")
        .arg("--json")
        .output()
        .expect("JSON doctor should run");

    assert_code(&human, 0);
    assert_code(&machine, 0);
    assert_ne!(human.stdout, Vec::<u8>::new());
    let report = json(&machine);
    for key in [
        "schema",
        "integrity_check",
        "foreign_key_check",
        "orphans",
        "holders",
        "vacuum_headroom",
    ] {
        assert!(report.get(key).is_some(), "missing doctor section {key}");
    }
}

#[test]
fn clean_dry_run_and_apply_match_exact_counts_and_identifiers() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 6,
        archived_session_count: 3,
        messages_per_session: 2,
        parts_per_message: 2,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    let before_ids = session_ids(&fixture.database_path);
    let selected_ids = super::support::archived_session_ids(&fixture.database_path);

    let dry_run = command(&fixture, "clean")
        .args([
            "--archived",
            "--keep-recent",
            "0",
            "--no-vacuum",
            "--json",
            "--dry-run",
        ])
        .output()
        .expect("clean dry-run should run");
    let applied = command(&fixture, "clean")
        .args([
            "--archived",
            "--keep-recent",
            "0",
            "--no-vacuum",
            "--json",
            "--dangerously-skip-confirm",
        ])
        .output()
        .expect("clean apply should run");

    assert_code(&dry_run, 0);
    assert_code(&applied, 0);
    let dry_run = json(&dry_run);
    let applied = json(&applied);
    assert_eq!(applied["impact"], dry_run["impact"]);
    assert_eq!(
        applied["deleted_sessions"],
        dry_run["impact"]["total_sessions"]
    );
    let predicted_rows: u64 = dry_run["impact"]["table_rows"]
        .as_object()
        .expect("table rows should be an object")
        .values()
        .map(|value| value.as_u64().expect("table count should be unsigned"))
        .sum();
    assert_eq!(applied["deleted_rows"], predicted_rows);
    assert_eq!(
        dry_run["impact"]["total_sessions"],
        u64::try_from(selected_ids.len()).expect("selected count should fit u64")
    );
    assert_eq!(
        session_ids(&fixture.database_path),
        before_ids.difference(&selected_ids).cloned().collect()
    );
}

#[test]
fn vacuum_dry_run_is_byte_identical_and_apply_shrinks_without_data_loss() {
    let fixture = Fixture::build(&FixtureConfig {
        session_count: 12,
        messages_per_session: 2,
        parts_per_message: 2,
        blob_size_per_part: 64 * 1_024,
        ..FixtureConfig::default()
    })
    .expect("fixture should build");
    fixture
        .connect()
        .expect("fixture should connect")
        .execute(
            "DELETE FROM session WHERE id = ?1",
            [&fixture.session_ids[0]],
        )
        .expect("one session should delete to create freelist space");
    let before_hash = file_hash(&fixture.database_path);
    let before_size = fs::metadata(&fixture.database_path)
        .expect("fixture metadata should read")
        .len();
    let before_rows = table_counts(&fixture.database_path);

    let dry_run = command(&fixture, "vacuum")
        .args(["--json", "--dry-run"])
        .output()
        .expect("vacuum dry-run should run");
    assert_code(&dry_run, 0);
    assert_eq!(file_hash(&fixture.database_path), before_hash);
    let preview = json(&dry_run);
    assert_eq!(preview["mode"], "dry-run");

    let applied = command(&fixture, "vacuum")
        .args(["--json", "--dangerously-skip-confirm"])
        .output()
        .expect("vacuum apply should run");

    assert_code(&applied, 0);
    let applied = json(&applied);
    assert_eq!(applied["mode"], "applied");
    assert_eq!(applied["current_size"], preview["current_size"]);
    assert_eq!(applied["live_bytes"], preview["live_bytes"]);
    assert_eq!(applied["freelist_bytes"], preview["freelist_bytes"]);
    assert!(
        fs::metadata(&fixture.database_path)
            .expect("fixture metadata should read")
            .len()
            < before_size
    );
    assert_eq!(table_counts(&fixture.database_path), before_rows);

    let backup_names = fs::read_dir(fixture.root())
        .expect("fixture root should read")
        .map(|entry| entry.expect("fixture entry should read").file_name())
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name.starts_with("opencode.db.bak."))
        .collect::<BTreeSet<_>>();
    assert_eq!(backup_names.len(), 1);
    let backup = backup_names.iter().next().expect("backup should exist");
    let timestamp = backup
        .strip_prefix("opencode.db.bak.")
        .expect("backup should use the database name");
    assert_eq!(timestamp.len(), 16);
    assert_eq!(&timestamp[8..9], "T");
    assert_eq!(&timestamp[15..16], "Z");
    assert!(timestamp[..8].bytes().all(|byte| byte.is_ascii_digit()));
    assert!(timestamp[9..15].bytes().all(|byte| byte.is_ascii_digit()));
    assert!(!backup.bytes().any(|byte| b":*?\"<>|".contains(&byte)));
    assert_eq!(row_count(&fixture.database_path, "session"), 11);
}
