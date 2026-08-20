#[allow(dead_code)]
#[path = "support/fixture.rs"]
mod fixture;

mod db {

    use std::fs;
    use std::time::Duration;

    use super::fixture::{Fixture, FixtureConfig};
    use rusqlite::{Connection, ErrorCode, OpenFlags, params};

    use oc_clean::db::{ConnectionOptions, open_read_only, open_read_write};
    use oc_clean::error::Error;
    use oc_clean::paths::Target;

    fn fixture_target(fixture: &Fixture) -> Target {
        Target::File(fixture.database_path.clone())
    }

    fn count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count query should succeed")
    }

    fn is_locked(error: &rusqlite::Error) -> bool {
        matches!(
            error.sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        )
    }

    #[test]
    fn constructor_applies_required_connection_pragmas() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let options = ConnectionOptions {
            busy_timeout: Duration::from_millis(37),
            ..ConnectionOptions::default()
        };
        let database = open_read_write(&fixture_target(&fixture), options).unwrap();

        assert_eq!(
            database
                .connection()
                .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            database
                .connection()
                .pragma_query_value(None, "busy_timeout", |row| row.get::<_, i64>(0))
                .unwrap(),
            37
        );
        assert_eq!(
            database
                .connection()
                .pragma_query_value(None, "temp_store", |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            database
                .connection()
                .pragma_query_value(None, "cache_size", |row| row.get::<_, i64>(0))
                .unwrap(),
            i64::from(options.cache_size)
        );
        assert_eq!(ConnectionOptions::default().cache_size, -64_000);
    }

    #[test]
    fn in_memory_write_connection_has_no_file_capability_or_identity() {
        let database = open_read_write(&Target::Memory, ConnectionOptions::default()).unwrap();

        assert!(!database.capabilities().hard_links);
        assert!(matches!(
            database.file_identity(),
            Err(Error::InvalidArgument { argument, .. }) if argument == ":memory:"
        ));
        assert!(matches!(
            open_read_only(&Target::Memory, ConnectionOptions::default()),
            Err(Error::InvalidArgument { argument, .. }) if argument == ":memory:"
        ));
    }

    #[test]
    fn fixture_reports_capabilities_and_hard_link_probe_cleans_up() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let entries_before = fs::read_dir(fixture.root()).unwrap().count();
        let database =
            open_read_only(&fixture_target(&fixture), ConnectionOptions::default()).unwrap();
        let capabilities = database.capabilities();

        assert!(capabilities.octet_length);
        assert!(capabilities.dbstat);
        assert!(capabilities.hard_links);
        let version = capabilities
            .sqlite_version
            .split('.')
            .map(|part| part.parse::<u32>().unwrap())
            .collect::<Vec<_>>();
        assert!(version.as_slice() >= &[3, 43]);
        assert_eq!(
            fs::read_dir(fixture.root()).unwrap().count(),
            entries_before
        );
    }

    #[test]
    fn foreign_keys_pragma_controls_session_cascades() {
        let enabled_fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let enabled = open_read_write(
            &fixture_target(&enabled_fixture),
            ConnectionOptions::default(),
        )
        .unwrap();
        enabled
            .connection()
            .execute(
                "DELETE FROM session WHERE id = ?1",
                params![enabled_fixture.session_ids[0]],
            )
            .unwrap();
        assert_eq!(count(enabled.connection(), "message"), 0);
        assert_eq!(count(enabled.connection(), "part"), 0);

        let disabled_fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let disabled = Connection::open(&disabled_fixture.database_path).unwrap();
        disabled.pragma_update(None, "foreign_keys", false).unwrap();
        assert_eq!(
            disabled
                .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        disabled
            .execute(
                "DELETE FROM session WHERE id = ?1",
                params![disabled_fixture.session_ids[0]],
            )
            .unwrap();
        assert_eq!(count(&disabled, "message"), 1);
        assert_eq!(count(&disabled, "part"), 1);
    }

    #[test]
    fn read_only_connection_rejects_insert() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let database =
            open_read_only(&fixture_target(&fixture), ConnectionOptions::default()).unwrap();
        let result = database.connection().execute(
        "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) VALUES ('blocked', '/tmp/blocked', 0, 0, '[]')",
        [],
    );

        assert!(result.is_err());
        drop(database.interrupt_handle());
    }

    #[test]
    fn exclusive_lock_blocks_external_reads_and_writes() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let database =
            open_read_write(&fixture_target(&fixture), ConnectionOptions::default()).unwrap();
        database.acquire_exclusive_lock().unwrap();

        let contender = Connection::open(&fixture.database_path).unwrap();
        contender.busy_timeout(Duration::ZERO).unwrap();
        let write_error = contender
            .execute("UPDATE session SET title = 'blocked'", [])
            .unwrap_err();
        let read_error = contender
            .query_row("SELECT count(*) FROM session", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_err();

        assert!(
            is_locked(&write_error),
            "unexpected write error: {write_error}"
        );
        assert!(
            is_locked(&read_error),
            "unexpected read error: {read_error}"
        );
    }

    #[test]
    fn exclusive_lock_contention_returns_typed_database_busy_error() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let blocker = Connection::open(&fixture.database_path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let database = open_read_write(
            &fixture_target(&fixture),
            ConnectionOptions {
                busy_timeout: Duration::ZERO,
                ..ConnectionOptions::default()
            },
        )
        .unwrap();

        let error = database.acquire_exclusive_lock().unwrap_err();

        assert!(matches!(error, Error::DatabaseBusy { .. }));
        assert_eq!(error.exit_code(), 5);
        blocker.execute_batch("ROLLBACK;").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_identity_preserves_path_context_when_metadata_disappears() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let database =
            open_read_only(&fixture_target(&fixture), ConnectionOptions::default()).unwrap();
        fs::remove_file(&fixture.database_path).unwrap();

        let error = database.file_identity().unwrap_err();

        assert!(matches!(error, Error::Io { path, .. } if path == fixture.database_path));
    }

    #[test]
    fn fresh_connections_share_data_version_while_file_identity_changes() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let target = fixture_target(&fixture);
        let before_connection = open_read_only(&target, ConnectionOptions::default()).unwrap();
        let before_version = before_connection.data_version().unwrap();
        let before_identity = before_connection.file_identity().unwrap();
        drop(before_connection);

        let writer = Connection::open(&fixture.database_path).unwrap();
        writer
        .execute(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) VALUES ('external', '/tmp/external', 0, 0, '[]')",
            [],
        )
        .unwrap();
        drop(writer);

        let after_connection = open_read_only(&target, ConnectionOptions::default()).unwrap();
        assert_eq!(after_connection.data_version().unwrap(), before_version);
        assert_ne!(after_connection.file_identity().unwrap(), before_identity);
    }

    #[test]
    fn read_only_report_connections_leave_concurrent_reader_unblocked() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let _report =
            open_read_only(&fixture_target(&fixture), ConnectionOptions::default()).unwrap();
        let reader = Connection::open_with_flags(
            &fixture.database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        reader.busy_timeout(Duration::ZERO).unwrap();

        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM session", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn nonexistent_database_returns_typed_not_found_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let missing = temp_dir.path().join("missing.db");
        let error = open_read_only(&Target::File(missing.clone()), ConnectionOptions::default())
            .unwrap_err();

        assert!(matches!(&error, Error::NotFound { path } if path == &missing));
        assert_eq!(error.exit_code(), 3);
    }
}
