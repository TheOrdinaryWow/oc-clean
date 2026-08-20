#[path = "support/fixture.rs"]
mod fixture;

use std::collections::BTreeMap;
use std::fs;

use fixture::{Fixture, FixtureConfig, SchemaShape};
use rusqlite::params;

const EXPECTED_FOREIGN_KEYS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "account_state",
        "active_account_id",
        "account",
        "id",
        "SET NULL",
    ),
    (
        "event",
        "aggregate_id",
        "event_sequence",
        "aggregate_id",
        "CASCADE",
    ),
    ("message", "session_id", "session", "id", "CASCADE"),
    ("part", "message_id", "message", "id", "CASCADE"),
    ("permission", "project_id", "project", "id", "CASCADE"),
    (
        "project_directory",
        "project_id",
        "project",
        "id",
        "CASCADE",
    ),
    ("session", "project_id", "project", "id", "CASCADE"),
    (
        "session_context_epoch",
        "session_id",
        "session",
        "id",
        "CASCADE",
    ),
    ("session_input", "session_id", "session", "id", "CASCADE"),
    ("session_message", "session_id", "session", "id", "CASCADE"),
    ("session_share", "session_id", "session", "id", "CASCADE"),
    ("todo", "session_id", "session", "id", "CASCADE"),
    ("workspace", "project_id", "project", "id", "CASCADE"),
];

fn count(connection: &rusqlite::Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count query should succeed")
}

#[test]
fn fixture_schema_matches_committed_ddl_for_both_shapes() {
    for shape in [SchemaShape::Upgraded, SchemaShape::Fresh] {
        let fixture = Fixture::build(&FixtureConfig {
            shape,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        fixture::assert_schema_matches_committed_ddl(&fixture.connect().unwrap(), shape)
            .expect("schema should match committed DDL");
    }
}

#[test]
fn fixture_has_exact_foreign_keys_and_passes_integrity_check() {
    let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
    let connection = fixture.connect().unwrap();
    let mut actual = Vec::new();
    for table in fixture::TABLES {
        let mut statement = connection
            .prepare(&format!("PRAGMA foreign_key_list('{table}')"))
            .unwrap();
        actual.extend(
            statement
                .query_map([], |row| {
                    Ok((
                        *table,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(6)?,
                    ))
                })
                .unwrap()
                .map(Result::unwrap),
        );
    }
    actual.sort();
    let mut expected = EXPECTED_FOREIGN_KEYS
        .iter()
        .map(|&(table, from, target, to, action)| {
            (
                table,
                from.to_owned(),
                target.to_owned(),
                to.to_owned(),
                action.to_owned(),
            )
        })
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
}

#[test]
fn fixture_deleting_session_cascades_relations_and_preserves_events() {
    let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
    let connection = fixture.connect().unwrap();
    let session_id = &fixture.session_ids[0];
    connection
        .execute("DELETE FROM session WHERE id = ?1", params![session_id])
        .unwrap();
    for table in ["message", "part", "session_message", "todo"] {
        assert_eq!(count(&connection, table), 0, "{table} should cascade");
    }
    assert_eq!(count(&connection, "event"), 1);
}

#[test]
fn ten_session_fixture_matches_parameterized_row_counts() {
    let config = FixtureConfig {
        project_count: 2,
        session_count: 10,
        messages_per_session: 3,
        parts_per_message: 2,
        blob_size_per_part: 128,
        archived_session_count: 4,
        time_span_ms: 10_000,
        ..FixtureConfig::default()
    };
    let fixture = Fixture::build(&config).unwrap();
    let connection = fixture.connect().unwrap();
    let sessions = i64::try_from(config.expected_session_count()).unwrap();
    let messages = sessions * i64::try_from(config.messages_per_session).unwrap();
    let parts = messages * i64::try_from(config.parts_per_message).unwrap();
    let expected = BTreeMap::from([
        ("project", i64::try_from(config.project_count).unwrap()),
        ("session", sessions),
        ("message", messages),
        ("part", parts),
        ("session_message", messages),
        ("session_input", sessions),
        ("session_context_epoch", sessions),
        ("session_share", sessions),
        ("todo", sessions),
        ("event_sequence", sessions),
        ("event", sessions),
    ]);
    for (table, expected_count) in expected {
        assert_eq!(count(&connection, table), expected_count, "{table}");
    }
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM session WHERE time_archived IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        4
    );
    let part_size: i64 = connection
        .query_row(
            "SELECT length(json_extract(data, '$.blob')) FROM part LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(part_size, 128);
}

#[test]
fn fixture_orphan_events_are_queryable_by_missing_session() {
    let fixture = Fixture::build(&FixtureConfig {
        orphan_event_count: 3,
        ..FixtureConfig::default()
    })
    .unwrap();
    let connection = fixture.connect().unwrap();
    let orphans: i64 = connection
        .query_row(
            "SELECT count(*) FROM event e LEFT JOIN session s ON s.id = e.aggregate_id WHERE s.id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 3);
}

#[test]
fn fixture_storage_and_snapshot_trees_contain_requested_orphans() {
    let fixture = Fixture::build(&FixtureConfig {
        project_count: 2,
        session_count: 3,
        orphan_storage_file_count: 4,
        orphan_snapshot_dir_count: 5,
        ..FixtureConfig::default()
    })
    .unwrap();
    assert!(fixture.root().is_dir());
    assert!(fixture.storage_dir.is_dir());
    assert!(fixture.snapshot_dir.is_dir());
    assert_eq!(fixture.orphan_storage_files.len(), 4);
    assert_eq!(fixture.orphan_snapshot_dirs.len(), 5);
    assert!(
        fixture
            .orphan_storage_files
            .iter()
            .all(|path| path.is_file())
    );
    assert!(fixture.orphan_snapshot_dirs.iter().all(|path| {
        path.is_dir()
            && path.join("HEAD").is_file()
            && path.join("objects").is_dir()
            && path.join("refs").is_dir()
    }));
    let storage_files = fs::read_dir(fixture.storage_dir.join("session"))
        .unwrap()
        .count();
    assert_eq!(storage_files, 7);
}
