use rusqlite::{Connection, params};

use crate::db::{Capabilities, DatabaseConnection};
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
SELECT 'project', project.id, '', 0, COALESCE(SUM(self_bytes.bytes), 0)
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
    pub bytes: u64,
}

/// Payload bytes owned by one session and by its complete descendant subtree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAttribution {
    pub session_id: String,
    pub project_id: String,
    pub self_bytes: u64,
    pub subtree_bytes: u64,
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
    let bytes = row
        .get::<_, i64>(4)
        .map_err(|source| sqlite_error("reading project-attributed bytes", source))?;
    Ok(ProjectAttribution {
        project_id,
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
    Error::Sqlite {
        context: context.to_owned(),
        source,
    }
}
