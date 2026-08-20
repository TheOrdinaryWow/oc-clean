//! Raw orphan discovery for the additive `--orphans` deletion phase.
//!
//! These selectors deliberately accept no age, project, or other session-scoped predicate. Their
//! results are added independently to the deletion plan. The integration layer for Todo 19/21 must
//! subtract the `--keep-recent` retention set before deletion; retained dangling sessions and their
//! retained descendants remain protected even though they appear in this module's raw output.
//!
//! Deleting a dangling-parent session can make its children dangling. [`dangling_session_ids`]
//! therefore follows descendants from every initially dangling session and returns the complete
//! fixed-point set in one call.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::db::DatabaseConnection;
use crate::error::Error;
use crate::paths::DerivedPaths;

/// An `event_sequence.aggregate_id` with session shape and no live session.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EventAggregateId(String);

impl EventAggregateId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A session selected because its parent is absent or transitively selected.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DanglingSessionId(String);

impl DanglingSessionId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Raw orphan candidates before retention-set filtering.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RawOrphans {
    pub event_aggregate_ids: BTreeSet<EventAggregateId>,
    pub dangling_session_ids: BTreeSet<DanglingSessionId>,
    pub storage_files: BTreeSet<PathBuf>,
    pub snapshot_directories: BTreeSet<PathBuf>,
}

/// Discovers every raw orphan class without applying session-scoped predicates or retention.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when an identifier query fails or [`Error::Io`] when an existing
/// storage or snapshot directory cannot be read.
pub fn select<Access>(
    database: &DatabaseConnection<Access>,
    paths: &DerivedPaths,
) -> Result<RawOrphans, Error> {
    let session_ids = string_ids(
        database.connection(),
        "SELECT id FROM session",
        "reading session identifiers for orphan selection",
    )?;
    let project_ids = string_ids(
        database.connection(),
        "SELECT id FROM project",
        "reading project identifiers for orphan selection",
    )?;

    Ok(RawOrphans {
        event_aggregate_ids: orphan_event_aggregate_ids(database)?,
        dangling_session_ids: dangling_session_ids(database)?,
        storage_files: orphan_storage_files(&paths.storage, &session_ids)?,
        snapshot_directories: orphan_snapshot_directories(&paths.snapshot, &project_ids)?,
    })
}

/// Returns session-shaped event aggregates whose session is absent.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when the query fails.
pub fn orphan_event_aggregate_ids<Access>(
    database: &DatabaseConnection<Access>,
) -> Result<BTreeSet<EventAggregateId>, Error> {
    const SQL: &str = "
        SELECT es.aggregate_id
        FROM event_sequence AS es
        LEFT JOIN session AS s ON s.id = es.aggregate_id
        WHERE s.id IS NULL
        ORDER BY es.aggregate_id
    ";
    Ok(string_ids(
        database.connection(),
        SQL,
        "querying orphan event aggregate identifiers",
    )?
    .into_iter()
    .filter(|id| is_session_id(id))
    .map(EventAggregateId)
    .collect())
}

/// Returns initially dangling sessions and all descendants they make dangling when deleted.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when the recursive query fails.
pub fn dangling_session_ids<Access>(
    database: &DatabaseConnection<Access>,
) -> Result<BTreeSet<DanglingSessionId>, Error> {
    const SQL: &str = "
        WITH RECURSIVE dangling(id) AS (
            SELECT session.id
            FROM session
            LEFT JOIN session AS parent ON parent.id = session.parent_id
            WHERE session.parent_id IS NOT NULL AND parent.id IS NULL
            UNION
            SELECT child.id
            FROM session AS child
            JOIN dangling AS selected_parent ON child.parent_id = selected_parent.id
        )
        SELECT id FROM dangling ORDER BY id
    ";
    Ok(string_ids(
        database.connection(),
        SQL,
        "querying dangling-parent session identifiers",
    )?
    .into_iter()
    .map(DanglingSessionId)
    .collect())
}

fn is_session_id(value: &str) -> bool {
    value.strip_prefix("ses_").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

fn orphan_storage_files(
    storage_root: &Path,
    session_ids: &BTreeSet<String>,
) -> Result<BTreeSet<PathBuf>, Error> {
    let mut paths = BTreeSet::new();
    for bucket in directory_entries(storage_root)? {
        if !entry_is_directory(&bucket)? {
            continue;
        }
        for entry in directory_entries(&bucket.path())? {
            if entry_is_file(&entry)?
                && storage_session_id(&entry.path()).is_some_and(|id| !session_ids.contains(id))
            {
                paths.insert(entry.path());
            }
        }
    }
    Ok(paths)
}

fn orphan_snapshot_directories(
    snapshot_root: &Path,
    project_ids: &BTreeSet<String>,
) -> Result<BTreeSet<PathBuf>, Error> {
    let mut paths = BTreeSet::new();
    for entry in directory_entries(snapshot_root)? {
        if entry_is_directory(&entry)? {
            let project_id = entry.file_name().to_string_lossy().into_owned();
            if !project_ids.contains(&project_id) {
                paths.insert(entry.path());
            }
        }
    }
    Ok(paths)
}

fn string_ids(
    connection: &Connection,
    sql: &str,
    context: &str,
) -> Result<BTreeSet<String>, Error> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|source| sqlite_error(context, source))?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(|source| sqlite_error(context, source))?
        .collect::<Result<_, _>>()
        .map_err(|source| sqlite_error(context, source))
}

fn directory_entries(path: &Path) -> Result<Vec<fs::DirEntry>, Error> {
    let read_dir = match fs::read_dir(path) {
        Ok(read_dir) => read_dir,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(path, source)),
    };
    let mut entries = read_dir
        .map(|entry| entry.map_err(|source| io_error(path, source)))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn entry_is_directory(entry: &fs::DirEntry) -> Result<bool, Error> {
    entry
        .file_type()
        .map(|file_type| file_type.is_dir())
        .map_err(|source| io_error(&entry.path(), source))
}

fn entry_is_file(entry: &fs::DirEntry) -> Result<bool, Error> {
    entry
        .file_type()
        .map(|file_type| file_type.is_file())
        .map_err(|source| io_error(&entry.path(), source))
}

fn storage_session_id(path: &Path) -> Option<&str> {
    (path.extension()? == "json")
        .then(|| path.file_stem()?.to_str())
        .flatten()
        .filter(|id| id.starts_with("ses_"))
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        context: context.to_owned(),
        source,
    }
}
