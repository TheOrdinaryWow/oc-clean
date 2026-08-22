use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{self, Capabilities, DatabaseConnection};
use crate::error::Error;

const ATTRIBUTION_SQL: &str = r"
WITH RECURSIVE
message_rollup(session_id, bytes) AS MATERIALIZED (
    SELECT session_id, COALESCE(SUM(octet_length(data)), 0)
    FROM message AS source
    GROUP BY session_id
),
part_rollup(session_id, bytes) AS MATERIALIZED (
    SELECT session_id, COALESCE(SUM(octet_length(data)), 0)
    FROM part AS source
    GROUP BY session_id
),
session_context_epoch_rollup(session_id, bytes) AS MATERIALIZED (
    SELECT session_id, COALESCE(SUM(octet_length(baseline) + octet_length(snapshot)), 0)
    FROM session_context_epoch AS source
    GROUP BY session_id
),
session_message_rollup(session_id, bytes) AS MATERIALIZED (
    SELECT session_id, COALESCE(SUM(octet_length(data)), 0)
    FROM session_message AS source
    GROUP BY session_id
),
event_rollup(session_id, bytes) AS MATERIALIZED (
    SELECT aggregate_id, COALESCE(SUM(octet_length(data)), 0)
    FROM event AS source
    GROUP BY aggregate_id
),
self_bytes(session_id, project_id, bytes) AS MATERIALIZED (
    SELECT
        session.id,
        session.project_id,
        COALESCE(message_rollup.bytes, 0)
            + COALESCE(part_rollup.bytes, 0)
            + COALESCE(session_context_epoch_rollup.bytes, 0)
            + COALESCE(session_message_rollup.bytes, 0)
            + COALESCE(event_rollup.bytes, 0)
    FROM session
    LEFT JOIN message_rollup ON message_rollup.session_id = session.id
    LEFT JOIN part_rollup ON part_rollup.session_id = session.id
    LEFT JOIN session_context_epoch_rollup
        ON session_context_epoch_rollup.session_id = session.id
    LEFT JOIN session_message_rollup ON session_message_rollup.session_id = session.id
    LEFT JOIN event_rollup ON event_rollup.session_id = session.id
),
subtree(root_id, session_id, path, cycle) AS (
    SELECT id, id, char(31) || id || char(31), 0
    FROM session
    UNION ALL
    SELECT
        subtree.root_id,
        child.id,
        subtree.path || child.id || char(31),
        instr(subtree.path, char(31) || child.id || char(31)) > 0
    FROM subtree
    JOIN session AS child ON child.parent_id = subtree.session_id
    WHERE subtree.cycle = 0
),
subtree_rollup(root_id, bytes, cycle) AS MATERIALIZED (
    SELECT subtree.root_id, SUM(self_bytes.bytes), MAX(subtree.cycle)
    FROM subtree
    JOIN self_bytes ON self_bytes.session_id = subtree.session_id
    GROUP BY subtree.root_id
),
top_sessions(session_id, project_id, self_bytes, subtree_bytes) AS MATERIALIZED (
    SELECT
        self_bytes.session_id,
        self_bytes.project_id,
        self_bytes.bytes,
        subtree_rollup.bytes
    FROM self_bytes
    JOIN subtree_rollup ON subtree_rollup.root_id = self_bytes.session_id
    ORDER BY subtree_rollup.bytes DESC, self_bytes.session_id ASC
    LIMIT ?1
)
SELECT 'cycle', root_id, '', 0, 0
FROM subtree_rollup
WHERE cycle != 0
UNION ALL
SELECT 'project', project.id, project.worktree, 0, COALESCE(SUM(self_bytes.bytes), 0)
FROM project
LEFT JOIN self_bytes ON self_bytes.project_id = project.id
GROUP BY project.id
UNION ALL
SELECT 'session', session_id, project_id, self_bytes, subtree_bytes
FROM top_sessions
";

/// Bytes attributable to one project through its sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectAttribution {
    pub project_id: String,
    /// Absolute worktree path of the project.
    ///
    /// A project identifier is a hash, so a rollup keyed only by it cannot tell an operator
    /// which checkout the bytes belong to. Unlike a session's project path this is never
    /// optional: the rollup is driven by the `project` table itself, and `worktree` is
    /// `NOT NULL` there.
    pub worktree: String,
    pub bytes: u64,
}

/// Payload bytes owned by one session and by its complete descendant subtree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAttribution {
    pub session_id: String,
    pub project_id: String,
    pub self_bytes: u64,
    pub subtree_bytes: u64,
    /// Descriptive fields, present only once [`describe`] has run for this session.
    pub details: Option<SessionDetails>,
}

/// Human-facing session facts that a size rollup alone cannot convey.
///
/// A session identifier is a random string, so a report that shows only identifiers gives an
/// operator no basis for deciding what to keep. The title and activity date do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDetails {
    pub title: String,
    pub time_updated_ms: i64,
    pub message_count: u64,
    /// Absolute worktree path of the owning project, when that project still exists.
    ///
    /// A project identifier is a hash, so it cannot tell an operator which checkout a session
    /// belongs to. The worktree path can.
    ///
    /// It stays optional because `PRAGMA foreign_keys` is per-connection in SQLite: the cascade
    /// from `session.project_id` only fires for a writer that enabled it, so a tool that did not
    /// can delete a project row and leave its sessions behind, naming a project that is gone.
    pub project_path: Option<String>,
}

// OpenCode stores messages under two coexisting models: the legacy `message` table and the
// event-sourced `session_message` projection. A session can be represented in either or both,
// and when both are populated they describe the same conversation. Summing them would report
// double the real message count, so the larger of the two is the honest answer.
const DESCRIBE_SQL: &str = r"
SELECT
    session.id,
    session.title,
    session.time_updated,
    MAX(
        (SELECT COUNT(*) FROM message WHERE message.session_id = session.id),
        (SELECT COUNT(*) FROM session_message WHERE session_message.session_id = session.id)
    ),
    project.worktree
FROM session
LEFT JOIN project ON project.id = session.project_id
WHERE session.id = ?1
";

/// Attaches titles, activity dates, and message counts to the given sessions.
///
/// The lookup is deliberately per-session rather than a join inside the attribution query:
/// callers such as size selection request an attribution rollup covering every session, and
/// counting messages for all of them would scan the whole `message` table for a report that
/// only ever displays a handful of rows.
///
/// A session that disappeared between the rollup and this lookup keeps `details` unset rather
/// than failing the report.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when the description query cannot be prepared or executed.
pub fn describe(connection: &Connection, sessions: &mut [SessionAttribution]) -> Result<(), Error> {
    if sessions.is_empty() {
        return Ok(());
    }
    let mut statement = connection
        .prepare(DESCRIBE_SQL)
        .map_err(|source| sqlite_error("preparing session description lookup", source))?;
    for session in sessions {
        let row = statement
            .query_row(params![session.session_id], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .optional()
            .map_err(|source| sqlite_error("reading session description", source))?;
        session.details =
            row.map(
                |(title, time_updated_ms, message_count, project_path)| SessionDetails {
                    title,
                    time_updated_ms,
                    message_count: u64::try_from(message_count).unwrap_or(0),
                    project_path,
                },
            );
    }
    Ok(())
}

/// Project totals and the largest session subtrees.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttributionReport {
    pub projects: Vec<ProjectAttribution>,
    pub sessions: Vec<SessionAttribution>,
}

/// Attributes payload bytes using the connection's capability snapshot.
///
/// `top_n` limits the session rollup only. Every project remains in the project rollup.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when `octet_length()` is unavailable or `top_n` exceeds
/// SQLite's signed integer range, [`Error::SchemaIncompatible`] when `session.parent_id` contains a
/// cycle, or [`Error::Sqlite`] when an attribution query or integer conversion fails.
pub fn analyze<Access>(
    database: &DatabaseConnection<Access>,
    top_n: usize,
) -> Result<AttributionReport, Error> {
    analyze_with_capabilities(database, database.capabilities(), top_n)
}

/// Attributes payload bytes using an explicit capability snapshot.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when `octet_length()` is unavailable or `top_n` exceeds
/// SQLite's signed integer range, [`Error::SchemaIncompatible`] when `session.parent_id` contains a
/// cycle, or [`Error::Sqlite`] when an attribution query or integer conversion fails.
pub fn analyze_with_capabilities<Access>(
    database: &DatabaseConnection<Access>,
    capabilities: &Capabilities,
    top_n: usize,
) -> Result<AttributionReport, Error> {
    if !capabilities.octet_length {
        return Err(Error::InvalidArgument {
            argument: "SQLite capabilities".to_owned(),
            reason: "size attribution requires octet_length()".to_owned(),
        });
    }
    let top_n = i64::try_from(top_n).map_err(|_| Error::InvalidArgument {
        argument: "top_n".to_owned(),
        reason: "value exceeds SQLite's signed integer range".to_owned(),
    })?;

    attribution_report(database.connection(), top_n)
}

fn attribution_report(connection: &Connection, top_n: i64) -> Result<AttributionReport, Error> {
    let mut statement = connection
        .prepare(ATTRIBUTION_SQL)
        .map_err(|source| sqlite_error("preparing project and session size attribution", source))?;
    let mut rows = statement
        .query(params![top_n])
        .map_err(|source| sqlite_error("querying project and session size attribution", source))?;
    let mut projects = Vec::new();
    let mut sessions = Vec::new();

    while let Some(row) = rows
        .next()
        .map_err(|source| sqlite_error("reading project and session size attribution", source))?
    {
        let kind = row
            .get::<_, String>(0)
            .map_err(|source| sqlite_error("reading attribution row kind", source))?;
        let id = row
            .get::<_, String>(1)
            .map_err(|source| sqlite_error("reading attribution identifier", source))?;
        match kind.as_str() {
            "cycle" => return Err(parent_cycle(&id)),
            "project" => projects.push(project_row(row, id)?),
            "session" => sessions.push(session_row(row, id)?),
            _ => {
                return Err(sqlite_error(
                    "reading attribution row kind",
                    rusqlite::Error::InvalidColumnType(
                        0,
                        "kind".to_owned(),
                        rusqlite::types::Type::Text,
                    ),
                ));
            }
        }
    }

    sort_report(&mut projects, &mut sessions);
    Ok(AttributionReport { projects, sessions })
}

fn project_row(row: &rusqlite::Row<'_>, project_id: String) -> Result<ProjectAttribution, Error> {
    let worktree = row
        .get::<_, String>(2)
        .map_err(|source| sqlite_error("reading project worktree", source))?;
    let bytes = row
        .get::<_, i64>(4)
        .map_err(|source| sqlite_error("reading project-attributed bytes", source))?;
    Ok(ProjectAttribution {
        project_id,
        worktree,
        bytes: non_negative_bytes(4, bytes, "reading project-attributed bytes")?,
    })
}

fn session_row(row: &rusqlite::Row<'_>, session_id: String) -> Result<SessionAttribution, Error> {
    let project_id = row
        .get::<_, String>(2)
        .map_err(|source| sqlite_error("reading attributed session project", source))?;
    let self_bytes = row
        .get::<_, i64>(3)
        .map_err(|source| sqlite_error("reading session self bytes", source))?;
    let subtree_bytes = row
        .get::<_, i64>(4)
        .map_err(|source| sqlite_error("reading session subtree bytes", source))?;
    Ok(SessionAttribution {
        session_id,
        project_id,
        self_bytes: non_negative_bytes(3, self_bytes, "reading session self bytes")?,
        subtree_bytes: non_negative_bytes(4, subtree_bytes, "reading session subtree bytes")?,
        details: None,
    })
}

fn non_negative_bytes(column: usize, bytes: i64, context: &str) -> Result<u64, Error> {
    u64::try_from(bytes).map_err(|_| {
        sqlite_error(
            context,
            rusqlite::Error::IntegralValueOutOfRange(column, bytes),
        )
    })
}

fn sort_report(projects: &mut [ProjectAttribution], sessions: &mut [SessionAttribution]) {
    projects.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.project_id.cmp(&right.project_id))
    });
    sessions.sort_by(|left, right| {
        right
            .subtree_bytes
            .cmp(&left.subtree_bytes)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
}

fn parent_cycle(root_id: &str) -> Error {
    Error::SchemaIncompatible {
        incompatibility: format!(
            "session.parent_id cycle detected while traversing descendants from `{root_id}`"
        ),
    }
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
    use super::fixture::{Fixture, FixtureConfig};
    use super::*;
    use crate::db::{ConnectionOptions, open_read_only};
    use crate::paths::Target;

    fn described(session_count: usize, messages_per_session: usize) -> Vec<SessionAttribution> {
        let fixture = Fixture::build(&FixtureConfig {
            session_count,
            messages_per_session,
            parts_per_message: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let database = open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only");

        let mut sessions = analyze(&database, session_count)
            .expect("attribution should succeed")
            .sessions;
        describe(database.connection(), &mut sessions).expect("description should succeed");
        sessions
    }

    #[test]
    fn description_attaches_the_title_activity_date_and_message_count() {
        let sessions = described(2, 3);

        assert_eq!(sessions.len(), 2);
        for session in &sessions {
            let details = session
                .details
                .as_ref()
                .expect("a live session should be described");
            assert!(
                !details.title.is_empty(),
                "OpenCode's schema declares session.title NOT NULL"
            );
            assert!(details.time_updated_ms > 0);
            assert_eq!(
                details.message_count, 3,
                "the fixture writes each message under both storage models, and the count \
                 must report the conversation's real length rather than their sum"
            );
        }
    }

    #[test]
    fn the_project_rollup_carries_each_project_worktree_path() {
        let fixture = Fixture::build(&FixtureConfig {
            project_count: 2,
            session_count: 2,
            messages_per_session: 1,
            parts_per_message: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let database = open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only");

        let projects = analyze(&database, 2)
            .expect("attribution should succeed")
            .projects;

        assert_eq!(projects.len(), 2);
        for project in &projects {
            assert!(
                project.worktree.starts_with('/'),
                "a worktree is an absolute path, got `{}`",
                project.worktree
            );
        }
        // A project with no sessions still belongs in the rollup, at zero bytes.
        assert!(
            projects.iter().all(|project| !project.worktree.is_empty()),
            "`project.worktree` is NOT NULL, so no row may report an empty path"
        );
    }

    #[test]
    fn description_carries_the_owning_project_worktree_path() {
        let sessions = described(2, 1);

        for session in &sessions {
            let details = session
                .details
                .as_ref()
                .expect("a live session should be described");
            let path = details
                .project_path
                .as_ref()
                .expect("a session whose project row exists should carry its worktree");
            assert!(
                path.starts_with('/'),
                "a worktree is an absolute path, got `{path}`"
            );
        }
    }

    #[test]
    fn a_session_whose_project_row_is_gone_is_still_described_without_a_path() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 1,
            messages_per_session: 1,
            parts_per_message: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");

        // `PRAGMA foreign_keys` is per-connection, so a writer that leaves it off can delete a
        // project row without cascading to its sessions. This reproduces that leftover state.
        let writer = rusqlite::Connection::open(&fixture.database_path)
            .expect("fixture should open for writing");
        writer
            .pragma_update(None, "foreign_keys", "OFF")
            .expect("a writer may disable the cascade");
        writer
            .execute("DELETE FROM project", [])
            .expect("deleting the project without cascade should succeed");
        drop(writer);

        let database = open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only");

        // The size rollup keys off the project, so the orphaned session is addressed directly.
        let session_id: String = database
            .connection()
            .query_row("SELECT id FROM session LIMIT 1", [], |row| row.get(0))
            .expect("the session row should survive the uncascaded delete");
        let mut sessions = vec![SessionAttribution {
            session_id,
            project_id: "prj_vanished".to_owned(),
            self_bytes: 0,
            subtree_bytes: 0,
            details: None,
        }];
        describe(database.connection(), &mut sessions).expect("description should succeed");

        let details = sessions[0]
            .details
            .as_ref()
            .expect("the session itself still exists");
        assert!(
            details.project_path.is_none(),
            "a missing project row cannot supply a path"
        );
        assert!(!details.title.is_empty(), "the session is still described");
    }

    #[test]
    fn a_session_that_disappeared_is_left_undescribed_rather_than_failing() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 1,
            messages_per_session: 1,
            parts_per_message: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let database = open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only");

        let mut sessions = vec![SessionAttribution {
            session_id: "ses_vanished".to_owned(),
            project_id: "prj_vanished".to_owned(),
            self_bytes: 0,
            subtree_bytes: 0,
            details: None,
        }];
        describe(database.connection(), &mut sessions).expect("a missing row is not a failure");

        assert!(sessions[0].details.is_none());
    }

    #[test]
    fn describing_an_empty_slice_touches_the_database_not_at_all() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let database = open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only");

        describe(database.connection(), &mut []).expect("an empty slice is a no-op");
    }

    #[test]
    fn sqlite_busy_maps_to_exit_five() {
        let source = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );

        assert_eq!(
            sqlite_error("testing attribution query", source).exit_code(),
            5
        );
    }
}
