use std::collections::BTreeSet;

use rusqlite::{Transaction, TransactionBehavior};

use crate::db::{self, DatabaseConnection, ReadWrite};
use crate::error::Error;
use crate::select::predicates::SessionIds;

const DELETE_EMPTY_PROJECTS_SQL: &str = r"
DELETE FROM project
WHERE NOT EXISTS (
    SELECT 1
    FROM session
    WHERE session.project_id = project.id
)
AND (
    ?1
    OR EXISTS (
        SELECT 1
        FROM project_prune_candidates
        WHERE project_prune_candidates.id = project.id
    )
)
RETURNING id
";

/// A deterministic set of project identifiers.
pub type ProjectIds = BTreeSet<String>;

/// Returns project owners for sessions before those sessions are deleted.
///
/// # Errors
///
/// Returns a typed SQLite error when the ownership query fails.
pub fn owners_of_sessions(
    database: &DatabaseConnection<ReadWrite>,
    session_ids: &SessionIds,
) -> Result<ProjectIds, Error> {
    let mut owners = ProjectIds::new();
    let mut statement = database
        .connection()
        .prepare("SELECT project_id FROM session WHERE id = ?1")
        .map_err(|source| sqlite_error("preparing deleted-session project owners", source))?;
    for session_id in session_ids {
        let project_id = statement
            .query_row([session_id], |row| row.get::<_, String>(0))
            .map_err(|source| sqlite_error("reading deleted-session project owner", source))?;
        owners.insert(project_id);
    }
    Ok(owners)
}

/// Returns every current project identifier.
///
/// # Errors
///
/// Returns a typed SQLite error when the project census fails.
pub fn all_ids(database: &DatabaseConnection<ReadWrite>) -> Result<ProjectIds, Error> {
    let mut statement = database
        .connection()
        .prepare("SELECT id FROM project ORDER BY id")
        .map_err(|source| sqlite_error("preparing project census", source))?;
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| sqlite_error("querying project census", source))?
        .collect::<Result<ProjectIds, _>>()
        .map_err(|source| sqlite_error("reading project census", source))
}

/// Prunes projects that are empty after session deletion.
///
/// By default, `affected_project_ids` narrows pruning to projects that owned a session deleted by
/// this run. When `prune_preexisting_empty_projects` is true, every empty project is eligible.
/// Emptiness is always evaluated against the current `session` table, so a project with any
/// surviving session, including one protected by retention, remains intact.
///
/// # Errors
///
/// Returns a typed error when foreign keys are disabled or SQLite cannot materialize candidates or
/// delete empty projects.
pub fn prune(
    database: &DatabaseConnection<ReadWrite>,
    affected_project_ids: &ProjectIds,
    prune_preexisting_empty_projects: bool,
) -> Result<ProjectIds, Error> {
    ensure_foreign_keys(database)?;
    let transaction =
        Transaction::new_unchecked(database.connection(), TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error("starting empty-project pruning", source))?;
    transaction
        .execute_batch(
            "DROP TABLE IF EXISTS project_prune_candidates;
             CREATE TEMP TABLE project_prune_candidates(id TEXT PRIMARY KEY);",
        )
        .map_err(|source| sqlite_error("creating project-pruning candidate table", source))?;
    {
        let mut insert = transaction
            .prepare("INSERT INTO project_prune_candidates(id) VALUES (?1)")
            .map_err(|source| sqlite_error("preparing project-pruning candidates", source))?;
        for project_id in affected_project_ids {
            insert.execute([project_id]).map_err(|source| {
                sqlite_error("materializing project-pruning candidates", source)
            })?;
        }
    }
    let pruned = {
        let mut delete = transaction
            .prepare(DELETE_EMPTY_PROJECTS_SQL)
            .map_err(|source| sqlite_error("preparing empty-project pruning", source))?;
        delete
            .query_map([prune_preexisting_empty_projects], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|source| sqlite_error("deleting empty projects", source))?
            .collect::<Result<ProjectIds, _>>()
            .map_err(|source| sqlite_error("reading pruned project ids", source))?
    };
    transaction
        .execute("DROP TABLE project_prune_candidates", [])
        .map_err(|source| sqlite_error("clearing project-pruning candidates", source))?;
    transaction
        .commit()
        .map_err(|source| sqlite_error("committing empty-project pruning", source))?;
    Ok(pruned)
}

fn ensure_foreign_keys(database: &DatabaseConnection<ReadWrite>) -> Result<(), Error> {
    let enabled = database
        .connection()
        .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
        .map_err(|source| sqlite_error("verifying PRAGMA foreign_keys", source))?;
    if enabled != 1 {
        return Err(Error::IntegrityCheckFailed {
            check: "PRAGMA foreign_keys".to_owned(),
            message: "project pruning requires foreign_keys=1".to_owned(),
        });
    }
    Ok(())
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    db::sqlite_error(context, source)
}

#[cfg(test)]
#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rusqlite::Connection;

    use super::*;
    use crate::db::{ConnectionOptions, ReadWriteConnection, open_read_write};
    use crate::delete::sessions::{DeleteOptions, delete as delete_sessions};
    use crate::paths::Target;
    use crate::select::predicates::SessionIds;
    use crate::select::retention;

    use super::fixture::{Fixture, FixtureConfig};

    const PROJECT_RELATIONS: &[&str] = &["project_directory", "permission", "workspace"];

    fn open_fixture(fixture: &Fixture) -> ReadWriteConnection {
        open_read_write(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-write")
    }

    fn delete_options() -> DeleteOptions {
        DeleteOptions {
            batch_size: 10,
            batch_time_limit: Duration::from_secs(30),
        }
    }

    fn project_ids(values: &[&str]) -> ProjectIds {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn owners_of(database: &ReadWriteConnection, session_ids: &SessionIds) -> ProjectIds {
        session_ids
            .iter()
            .map(|session_id| {
                database
                    .connection()
                    .query_row(
                        "SELECT project_id FROM session WHERE id = ?1",
                        [session_id],
                        |row| row.get::<_, String>(0),
                    )
                    .expect("session owner should exist before deletion")
            })
            .collect()
    }

    fn count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("table should be countable")
    }

    fn count_project(connection: &Connection, project_id: &str) -> i64 {
        connection
            .query_row(
                "SELECT COUNT(*) FROM project WHERE id = ?1",
                [project_id],
                |row| row.get(0),
            )
            .expect("project should be countable")
    }

    fn count_project_relation(connection: &Connection, table: &str, project_id: &str) -> i64 {
        connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE project_id = ?1"),
                [project_id],
                |row| row.get(0),
            )
            .expect("project relation should be countable")
    }

    #[test]
    fn prunes_project_emptied_by_session_deletion_and_returns_exact_ids() {
        let fixture = Fixture::build(&FixtureConfig {
            project_count: 2,
            session_count: 2,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let database = open_fixture(&fixture);
        let deleted_sessions = [fixture.session_ids[0].clone()].into_iter().collect();
        let affected_projects = owners_of(&database, &deleted_sessions);

        delete_sessions(&database, &deleted_sessions, delete_options())
            .expect("session deletion should succeed");
        let pruned = prune(&database, &affected_projects, false)
            .expect("empty project pruning should succeed");

        assert_eq!(pruned, project_ids(&["project-0"]));
        assert_eq!(count_project(database.connection(), "project-0"), 0);
        assert_eq!(count_project(database.connection(), "project-1"), 1);
        for table in PROJECT_RELATIONS {
            assert_eq!(
                count_project_relation(database.connection(), table, "project-0"),
                0,
                "{table} should cascade"
            );
            assert_eq!(
                count_project_relation(database.connection(), table, "project-1"),
                1,
                "{table} for the live project should survive"
            );
        }
        assert_eq!(count(database.connection(), "account"), 1);
        assert_eq!(count(database.connection(), "credential"), 1);
        assert_eq!(count(database.connection(), "migration"), 1);
    }

    #[test]
    fn project_with_a_keep_recent_session_and_relations_survives() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 2,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let database = open_fixture(&fixture);
        let all_sessions: SessionIds = fixture.session_ids.iter().cloned().collect();
        let retained = retention::compute(&database, 1).expect("retention should compute");
        let deleted_sessions = all_sessions.difference(&retained).cloned().collect();
        let affected_projects = owners_of(&database, &deleted_sessions);

        delete_sessions(&database, &deleted_sessions, delete_options())
            .expect("unretained session deletion should succeed");
        let pruned =
            prune(&database, &affected_projects, false).expect("project pruning should succeed");

        assert!(pruned.is_empty());
        assert_eq!(count_project(database.connection(), "project-0"), 1);
        assert_eq!(count(database.connection(), "session"), 1);
        for table in PROJECT_RELATIONS {
            assert_eq!(
                count_project_relation(database.connection(), table, "project-0"),
                1,
                "{table} should survive with the retained session"
            );
        }
    }

    #[test]
    fn pre_existing_empty_project_requires_explicit_pruning() {
        let fixture = Fixture::build(&FixtureConfig {
            project_count: 2,
            session_count: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let database = open_fixture(&fixture);
        let deleted_sessions: SessionIds = fixture.session_ids.iter().cloned().collect();
        let affected_projects = owners_of(&database, &deleted_sessions);

        delete_sessions(&database, &deleted_sessions, delete_options())
            .expect("session deletion should succeed");
        let default_pruned = prune(&database, &affected_projects, false)
            .expect("default project pruning should succeed");

        assert_eq!(default_pruned, project_ids(&["project-0"]));
        assert_eq!(count_project(database.connection(), "project-0"), 0);
        assert_eq!(count_project(database.connection(), "project-1"), 1);

        let explicit_pruned = prune(&database, &ProjectIds::new(), true)
            .expect("explicit empty project pruning should succeed");

        assert_eq!(explicit_pruned, project_ids(&["project-1"]));
        assert_eq!(count_project(database.connection(), "project-1"), 0);
    }
}
