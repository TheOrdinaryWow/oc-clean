use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::process::{Command, Output};

use rusqlite::{Connection, params};
use serde_json::Value;

use super::fixture::Fixture;

pub(super) fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oc-clean"));
    command.env_remove("NO_COLOR").env_remove("OCC_DB");
    command
}

pub(super) fn command(fixture: &Fixture, subcommand: &str) -> Command {
    let mut command = binary();
    command
        .arg(subcommand)
        .arg("--db")
        .arg(&fixture.database_path);
    command
}

pub(super) fn assert_code(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(super) fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout should contain one JSON object: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

pub(super) fn file_hash(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    fs::read(path)
        .expect("fixture bytes should be readable")
        .hash(&mut hasher);
    hasher.finish()
}

pub(super) fn row_count(path: &Path, table: &str) -> u64 {
    let connection = Connection::open(path).expect("fixture should open");
    let quoted = table.replace('"', "\"\"");
    let count = connection
        .query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("table count should read");
    u64::try_from(count).expect("table count should be non-negative")
}

pub(super) fn table_counts(path: &Path) -> BTreeMap<String, u64> {
    let connection = Connection::open(path).expect("fixture should open");
    let mut tables = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("table query should prepare");
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))
        .expect("table query should run")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("table names should decode");
    names
        .into_iter()
        .map(|name| {
            let count = row_count(path, &name);
            (name, count)
        })
        .collect()
}

pub(super) fn session_ids(path: &Path) -> BTreeSet<String> {
    let connection = Connection::open(path).expect("fixture should open");
    let mut statement = connection
        .prepare("SELECT id FROM session ORDER BY id")
        .expect("session query should prepare");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("session query should run")
        .collect::<rusqlite::Result<_>>()
        .expect("session identifiers should decode")
}

pub(super) fn archived_session_ids(path: &Path) -> BTreeSet<String> {
    let connection = Connection::open(path).expect("fixture should open");
    let mut statement = connection
        .prepare("SELECT id FROM session WHERE time_archived IS NOT NULL ORDER BY id")
        .expect("archived-session query should prepare");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("archived-session query should run")
        .collect::<rusqlite::Result<_>>()
        .expect("archived session identifiers should decode")
}

pub(super) fn insert_dangling_session(fixture: &Fixture, id: &str, updated: i64) {
    let connection = fixture.connect().expect("fixture should connect");
    connection
        .execute(
            "INSERT INTO session (
                id, project_id, workspace_id, parent_id, slug, directory, path, title,
                version, time_created, time_updated, time_archived
             )
             SELECT ?1, project_id, workspace_id, ?2, ?1, directory, path, ?1,
                    version, ?3, ?3, NULL
             FROM session ORDER BY id LIMIT 1",
            params![id, format!("ses_Missing{id}"), updated],
        )
        .expect("dangling session should insert");
}

pub(super) fn max_session_time(fixture: &Fixture) -> i64 {
    fixture
        .connect()
        .expect("fixture should connect")
        .query_row("SELECT MAX(time_updated) FROM session", [], |row| {
            row.get(0)
        })
        .expect("maximum session timestamp should read")
}

pub(super) fn make_partial_success_fixture(fixture: &Fixture) {
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
}

pub(super) fn insert_foreign_key_violation(fixture: &Fixture) {
    let connection = fixture.connect().expect("fixture should connect");
    connection
        .execute_batch(
            "CREATE TABLE e2e_fk (
                id TEXT PRIMARY KEY,
                session_id TEXT REFERENCES session(id)
             );
             PRAGMA foreign_keys = OFF;
             INSERT INTO e2e_fk VALUES ('broken', 'ses_Missing');",
        )
        .expect("foreign-key violation should insert");
}
