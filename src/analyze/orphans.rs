use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use rusqlite::Connection;
use rusqlite::types::ValueRef;

use crate::assets::storage::session_id_from_path;
use crate::db::DatabaseConnection;
use crate::error::Error;
use crate::paths::DerivedPaths;

#[cfg(test)]
#[allow(dead_code)]
#[allow(clippy::duplicate_mod)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

/// Count and estimated payload bytes for one orphan class.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OrphanClass {
    pub count: u64,
    pub bytes: u64,
}

/// Read-only census of all orphan classes managed by `oc-clean`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OrphanReport {
    pub orphan_events: OrphanClass,
    pub dangling_parent_sessions: OrphanClass,
    pub foreign_key_dangling_rows: OrphanClass,
    pub orphan_storage_files: OrphanClass,
    pub orphan_snapshot_directories: OrphanClass,
}

/// Counts and sizes database and filesystem orphans without modifying them.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when a census query fails or [`Error::Io`] when a storage or snapshot
/// directory cannot be read.
pub fn analyze<Access>(
    database: &DatabaseConnection<Access>,
    paths: &DerivedPaths,
) -> Result<OrphanReport, Error> {
    let connection = database.connection();
    let session_ids = ids(
        connection,
        "SELECT id FROM session",
        "reading session identifiers",
    )?;
    let project_ids = ids(
        connection,
        "SELECT id FROM project",
        "reading project identifiers",
    )?;

    Ok(OrphanReport {
        orphan_events: orphan_events(connection)?,
        dangling_parent_sessions: dangling_parent_sessions(connection)?,
        foreign_key_dangling_rows: foreign_key_dangling_rows(connection)?,
        orphan_storage_files: orphan_storage_files(&paths.storage, &session_ids)?,
        orphan_snapshot_directories: orphan_snapshot_directories(&paths.snapshot, &project_ids)?,
    })
}

fn is_session_id(value: &str) -> bool {
    value.strip_prefix("ses_").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

fn orphan_events(connection: &Connection) -> Result<OrphanClass, Error> {
    const SQL: &str = "
        SELECT es.aggregate_id,
               octet_length(es.aggregate_id) + 8 + COALESCE(octet_length(es.owner_id), 0)
               + COALESCE(SUM(
                   octet_length(e.id) + octet_length(e.aggregate_id) + 8
                   + octet_length(e.type) + octet_length(e.data)
               ), 0)
        FROM event_sequence AS es
        LEFT JOIN session AS s ON s.id = es.aggregate_id
        LEFT JOIN event AS e ON e.aggregate_id = es.aggregate_id
        WHERE s.id IS NULL
        GROUP BY es.aggregate_id, es.seq, es.owner_id
        ORDER BY es.aggregate_id
    ";
    let mut statement = connection
        .prepare(SQL)
        .map_err(|source| sqlite_error("preparing orphan event census", source))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|source| sqlite_error("querying orphan events", source))?;
    let mut result = OrphanClass::default();
    for row in rows {
        let (aggregate_id, bytes) =
            row.map_err(|source| sqlite_error("reading orphan event census", source))?;
        if is_session_id(&aggregate_id) {
            result.count = result.count.saturating_add(1);
            result.bytes = result.bytes.saturating_add(to_u64(
                1,
                bytes,
                "reading orphan event payload bytes",
            )?);
        }
    }
    Ok(result)
}

fn dangling_parent_sessions(connection: &Connection) -> Result<OrphanClass, Error> {
    const SQL: &str = "
        SELECT COUNT(*),
               COALESCE(SUM(
                   octet_length(s.id) + octet_length(s.project_id)
                   + octet_length(s.parent_id) + octet_length(s.slug)
                   + octet_length(s.directory) + octet_length(s.title)
                   + octet_length(s.version) + COALESCE(octet_length(s.share_url), 0)
                   + COALESCE(octet_length(s.summary_diffs), 0)
                   + COALESCE(octet_length(s.revert), 0)
                   + COALESCE(octet_length(s.permission), 0)
                   + COALESCE(octet_length(s.workspace_id), 0)
                   + COALESCE(octet_length(s.path), 0)
                   + COALESCE(octet_length(s.agent), 0)
                   + COALESCE(octet_length(s.model), 0)
                   + COALESCE(octet_length(s.metadata), 0)
                   + 56
                   + CASE WHEN s.summary_additions IS NULL THEN 0 ELSE 8 END
                   + CASE WHEN s.summary_deletions IS NULL THEN 0 ELSE 8 END
                   + CASE WHEN s.summary_files IS NULL THEN 0 ELSE 8 END
                   + CASE WHEN s.time_compacting IS NULL THEN 0 ELSE 8 END
                   + CASE WHEN s.time_archived IS NULL THEN 0 ELSE 8 END
               ), 0)
        FROM session AS s
        LEFT JOIN session AS parent ON parent.id = s.parent_id
        WHERE s.parent_id IS NOT NULL AND parent.id IS NULL
    ";
    query_class(
        connection,
        SQL,
        "querying dangling-parent sessions",
        "reading dangling-parent session census",
    )
}

fn foreign_key_dangling_rows(connection: &Connection) -> Result<OrphanClass, Error> {
    let mut statement = connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(|source| sqlite_error("preparing PRAGMA foreign_key_check", source))?;
    let violations = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })
        .map_err(|source| sqlite_error("running PRAGMA foreign_key_check", source))?;
    let mut result = OrphanClass::default();
    for violation in violations {
        let (table, rowid) = violation
            .map_err(|source| sqlite_error("reading PRAGMA foreign_key_check result", source))?;
        result.count = result.count.saturating_add(1);
        if let Some(rowid) = rowid {
            result.bytes = result
                .bytes
                .saturating_add(database_row_bytes(connection, &table, rowid)?);
        }
    }
    Ok(result)
}

fn database_row_bytes(connection: &Connection, table: &str, rowid: i64) -> Result<u64, Error> {
    let quoted_table = table.replace('"', "\"\"");
    let sql = format!("SELECT * FROM \"{quoted_table}\" WHERE rowid = ?1");
    connection
        .query_row(&sql, [rowid], |row| {
            let mut bytes = 0_u64;
            for column in 0..row.as_ref().column_count() {
                bytes = bytes.saturating_add(value_bytes(row.get_ref(column)?));
            }
            Ok(bytes)
        })
        .map_err(|source| {
            sqlite_error(
                &format!("sizing foreign-key dangling row in `{table}`"),
                source,
            )
        })
}

fn value_bytes(value: ValueRef<'_>) -> u64 {
    match value {
        ValueRef::Null => 0,
        ValueRef::Integer(_) | ValueRef::Real(_) => 8,
        ValueRef::Text(value) | ValueRef::Blob(value) => {
            u64::try_from(value.len()).unwrap_or(u64::MAX)
        }
    }
}

fn orphan_storage_files(
    storage_root: &Path,
    session_ids: &BTreeSet<String>,
) -> Result<OrphanClass, Error> {
    let mut result = OrphanClass::default();
    for bucket in directory_entries(storage_root)? {
        if !entry_is_directory(&bucket)? {
            continue;
        }
        for entry in directory_entries(&bucket.path())? {
            if entry_is_file(&entry)?
                && session_id_from_path(&entry.path()).is_some_and(|id| !session_ids.contains(id))
            {
                result.count = result.count.saturating_add(1);
                result.bytes = result.bytes.saturating_add(metadata(&entry.path())?.len());
            }
        }
    }
    Ok(result)
}

fn orphan_snapshot_directories(
    snapshot_root: &Path,
    project_ids: &BTreeSet<String>,
) -> Result<OrphanClass, Error> {
    let mut result = OrphanClass::default();
    for entry in directory_entries(snapshot_root)? {
        if entry_is_directory(&entry)? {
            let project_id = entry.file_name().to_string_lossy().into_owned();
            if !project_ids.contains(&project_id) {
                result.count = result.count.saturating_add(1);
                result.bytes = result.bytes.saturating_add(directory_bytes(&entry.path())?);
            }
        }
    }
    Ok(result)
}

fn directory_bytes(path: &Path) -> Result<u64, Error> {
    let mut bytes = 0_u64;
    for entry in directory_entries(path)? {
        if entry_is_directory(&entry)? {
            bytes = bytes.saturating_add(directory_bytes(&entry.path())?);
        } else if entry_is_file(&entry)? {
            bytes = bytes.saturating_add(metadata(&entry.path())?.len());
        }
    }
    Ok(bytes)
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

fn metadata(path: &Path) -> Result<fs::Metadata, Error> {
    fs::metadata(path).map_err(|source| io_error(path, source))
}

fn ids(connection: &Connection, sql: &str, context: &str) -> Result<BTreeSet<String>, Error> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|source| sqlite_error(context, source))?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(|source| sqlite_error(context, source))?
        .collect::<Result<_, _>>()
        .map_err(|source| sqlite_error(context, source))
}

fn query_class(
    connection: &Connection,
    sql: &str,
    query_context: &str,
    read_context: &str,
) -> Result<OrphanClass, Error> {
    let (count, bytes) = connection
        .query_row(sql, [], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|source| sqlite_error(query_context, source))?;
    Ok(OrphanClass {
        count: to_u64(0, count, read_context)?,
        bytes: to_u64(1, bytes, read_context)?,
    })
}

fn to_u64(column: usize, value: i64, context: &str) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| {
        sqlite_error(
            context,
            rusqlite::Error::IntegralValueOutOfRange(column, value),
        )
    })
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

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::fixture::{Fixture, FixtureConfig};
    use super::*;
    use crate::db::{ConnectionOptions, open_read_only};
    use crate::paths::{Target, derived_paths};

    fn open_fixture(fixture: &Fixture) -> crate::db::ReadOnlyConnection {
        open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only")
    }

    fn insert_orphan_events(connection: &Connection, count: usize) {
        for index in 0..count {
            let aggregate_id = format!("ses_Orphan{index}");
            connection
                .execute(
                    "INSERT INTO event_sequence VALUES (?1, 1, NULL)",
                    [&aggregate_id],
                )
                .expect("orphan event sequence should insert");
            connection
                .execute(
                    "INSERT INTO event VALUES (?1, ?2, 1, 'session.orphan', '{}')",
                    params![format!("event-orphan-{index}"), aggregate_id],
                )
                .expect("orphan event should insert");
        }
    }

    fn census(fixture: &Fixture) -> OrphanReport {
        analyze(&open_fixture(fixture), &derived_paths(fixture.root()))
            .expect("orphan census should succeed")
    }

    #[test]
    fn reports_exact_counts_for_every_orphan_class() {
        let fixture = Fixture::build(&FixtureConfig {
            dangling_parent_session_count: 3,
            orphan_storage_file_count: 5,
            orphan_snapshot_dir_count: 2,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        insert_orphan_events(&fixture.connect().expect("fixture should connect"), 7);

        let report = census(&fixture);

        assert_eq!(report.orphan_events.count, 7);
        assert!(report.orphan_events.bytes > 0);
        assert_eq!(report.dangling_parent_sessions.count, 3);
        assert!(report.dangling_parent_sessions.bytes > 0);
        assert_eq!(report.foreign_key_dangling_rows, OrphanClass::default());
        assert_eq!(report.orphan_storage_files.count, 5);
        assert_eq!(report.orphan_storage_files.bytes, 10);
        assert_eq!(report.orphan_snapshot_directories.count, 2);
        assert!(report.orphan_snapshot_directories.bytes > 0);
    }

    #[test]
    fn excludes_non_session_aggregate_and_preserves_its_rows() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "INSERT INTO event_sequence VALUES ('prj_something', 1, NULL)",
                [],
            )
            .expect("project sequence should insert");
        connection
            .execute(
                "INSERT INTO event VALUES ('event-project', 'prj_something', 1, 'project.updated', '{}')",
                [],
            )
            .expect("project event should insert");
        drop(connection);

        let report = census(&fixture);
        let connection = fixture.connect().expect("fixture should reconnect");
        let sequence_count = count_aggregate_rows(&connection, "event_sequence", "prj_something");
        let event_count = count_aggregate_rows(&connection, "event", "prj_something");

        assert_eq!(report.orphan_events, OrphanClass::default());
        assert!(!is_session_id("prj_something"));
        assert_eq!((sequence_count, event_count), (1, 1));
    }

    #[test]
    fn foreign_key_check_discovers_new_foreign_key_without_module_changes() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .pragma_update(None, "foreign_keys", false)
            .expect("foreign keys should disable for corrupt fixture setup");
        connection
            .execute_batch(
                "CREATE TABLE plugin_parent (id TEXT PRIMARY KEY);\
                 CREATE TABLE plugin_child (\
                     id TEXT PRIMARY KEY,\
                     parent_id TEXT NOT NULL REFERENCES plugin_parent(id)\
                 );\
                 INSERT INTO plugin_child VALUES ('child-1', 'missing-parent');",
            )
            .expect("generic foreign-key violation should insert");
        drop(connection);

        let report = census(&fixture);

        assert_eq!(report.foreign_key_dangling_rows.count, 1);
        assert!(report.foreign_key_dangling_rows.bytes > 0);
    }

    #[test]
    fn census_succeeds_through_read_only_connection_without_writes() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let database = open_fixture(&fixture);
        let before = database.data_version().expect("data version should read");

        let report = analyze(&database, &derived_paths(fixture.root()))
            .expect("read-only census should succeed");

        assert_eq!(report, OrphanReport::default());
        assert_eq!(
            database.data_version().expect("data version should reread"),
            before
        );
        let write = database.connection().execute(
            "INSERT INTO event_sequence VALUES ('ses_forbidden', 1, NULL)",
            [],
        );
        assert!(write.is_err());
    }

    #[test]
    fn baseline_fixture_reports_zero_for_every_orphan_class() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");

        assert_eq!(census(&fixture), OrphanReport::default());
    }

    #[test]
    fn session_shaped_orphan_event_is_counted_and_sized() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "INSERT INTO event_sequence VALUES ('ses_Missing123', 9, 'owner')",
                [],
            )
            .expect("orphan event sequence should insert");
        connection
            .execute(
                "INSERT INTO event VALUES ('event-sized', 'ses_Missing123', 9, 'session.updated', ?1)",
                ["x".repeat(512)],
            )
            .expect("orphan event should insert");
        drop(connection);

        let report = census(&fixture);

        assert_eq!(report.orphan_events.count, 1);
        assert!(report.orphan_events.bytes >= 512);
    }

    #[test]
    fn session_id_shape_is_strict_ascii_alphanumeric() {
        for valid in ["ses_0", "ses_AbC123", "ses_Missing123"] {
            assert!(is_session_id(valid), "{valid}");
        }
        for invalid in [
            "ses_",
            "ses_has_underscore",
            "ses-hyphen",
            "ses_nonasciié",
            "prj_something",
        ] {
            assert!(!is_session_id(invalid), "{invalid}");
        }
    }

    fn count_aggregate_rows(connection: &Connection, table: &str, aggregate_id: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE aggregate_id = ?1");
        connection
            .query_row(&sql, [aggregate_id], |row| row.get(0))
            .expect("aggregate rows should count")
    }
}
