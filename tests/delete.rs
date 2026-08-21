#[allow(dead_code)]
#[path = "support/fixture.rs"]
mod fixture;

mod delete {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    use oc_clean::db::{ConnectionOptions, ReadWriteConnection, open_read_write};
    use oc_clean::delete::sessions::{DeleteOptions, delete};
    use oc_clean::error::Error;
    use oc_clean::paths::Target;
    use oc_clean::select::predicates::SessionIds;
    use rusqlite::Connection;

    use super::fixture::{Fixture, FixtureConfig};

    const SESSION_TABLES: &[&str] = &[
        "session",
        "message",
        "part",
        "session_message",
        "session_input",
        "session_context_epoch",
        "session_share",
        "todo",
        "event_sequence",
        "event",
    ];

    fn fixture(session_count: usize) -> Fixture {
        Fixture::build(&FixtureConfig {
            session_count,
            messages_per_session: 1,
            parts_per_message: 1,
            blob_size_per_part: 64 * 1_024,
            ..FixtureConfig::default()
        })
        .expect("fixture should build")
    }

    fn open_fixture(fixture: &Fixture) -> ReadWriteConnection {
        open_read_write(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-write")
    }

    fn ids(values: impl IntoIterator<Item = String>) -> SessionIds {
        values.into_iter().collect()
    }

    fn count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("table should be countable")
    }

    fn counts(connection: &Connection) -> BTreeMap<String, i64> {
        SESSION_TABLES
            .iter()
            .map(|table| ((*table).to_owned(), count(connection, table)))
            .collect()
    }

    fn options(batch_size: usize) -> DeleteOptions {
        DeleteOptions {
            batch_size,
            batch_time_limit: Duration::from_secs(30),
        }
    }

    #[test]
    fn deleting_sessions_removes_all_relations_and_reports_exact_counts() {
        let fixture = fixture(5);
        let database = open_fixture(&fixture);
        let selected = ids(fixture.session_ids[..2].iter().cloned());
        let before = counts(database.connection());

        let report = delete(&database, &selected, options(2)).expect("deletion should succeed");
        let after = counts(database.connection());

        for table in SESSION_TABLES {
            assert_eq!(before[*table] - after[*table], 2, "{table}");
            assert_eq!(report.table_rows[*table], 2, "{table}");
            assert_eq!(after[*table], 3, "{table}");
        }
        assert_eq!(report.transactions, 1);
    }

    #[test]
    fn foreign_keys_disabled_is_a_typed_refusal() {
        let fixture = fixture(1);
        let database = open_fixture(&fixture);
        database
            .connection()
            .pragma_update(None, "foreign_keys", false)
            .expect("foreign keys should turn off");

        let error = delete(
            &database,
            &ids(fixture.session_ids.iter().cloned()),
            DeleteOptions::default(),
        )
        .expect_err("deletion should refuse disabled foreign keys");

        assert!(matches!(
            error,
            Error::IntegrityCheckFailed { ref check, .. } if check == "PRAGMA foreign_keys"
        ));
        assert_eq!(count(database.connection(), "session"), 1);
    }

    #[test]
    fn shrinking_candidate_set_deletes_seven_sessions_in_four_transactions() {
        let fixture = fixture(7);
        let database = open_fixture(&fixture);

        let report = delete(
            &database,
            &ids(fixture.session_ids.iter().cloned()),
            options(2),
        )
        .expect("all batches should succeed");

        assert_eq!(report.transactions, 4);
        assert_eq!(report.table_rows["session"], 7);
        assert_eq!(count(database.connection(), "session"), 0);
    }

    #[test]
    fn purge_ids_is_empty_when_deletion_finishes() {
        let fixture = fixture(3);
        let database = open_fixture(&fixture);

        delete(
            &database,
            &ids(fixture.session_ids.iter().cloned()),
            options(2),
        )
        .expect("deletion should succeed");

        assert_eq!(count(database.connection(), "purge_ids"), 0);
    }

    #[test]
    fn foreign_key_check_is_clean_after_deletion() {
        let fixture = fixture(5);
        let database = open_fixture(&fixture);

        delete(
            &database,
            &ids(fixture.session_ids[..3].iter().cloned()),
            options(2),
        )
        .expect("deletion should succeed");

        assert_eq!(
            database
                .connection()
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("foreign key check should run"),
            0
        );
    }

    #[test]
    fn checkpointing_bounds_peak_wal_growth_across_many_batches() {
        let fixture = fixture(50);
        let setup = fixture.connect().expect("fixture should connect");
        setup
            .pragma_update(None, "journal_mode", "WAL")
            .expect("WAL mode should enable");
        drop(setup);
        let database_size = fs::metadata(&fixture.database_path)
            .expect("database metadata should exist")
            .len();
        let wal_path = fixture.database_path.with_extension("db-wal");
        let database = open_fixture(&fixture);
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let observer_stop = Arc::clone(&stop);
        let observer_peak = Arc::clone(&peak);
        let observer = thread::spawn(move || {
            while !observer_stop.load(Ordering::Relaxed) {
                let size = fs::metadata(&wal_path).map_or(0, |metadata| metadata.len());
                observer_peak.fetch_max(size, Ordering::Relaxed);
                thread::yield_now();
            }
        });

        delete(
            &database,
            &ids(fixture.session_ids.iter().cloned()),
            options(2),
        )
        .expect("many deletion batches should succeed");
        stop.store(true, Ordering::Relaxed);
        observer.join().expect("WAL observer should finish");

        assert!(
            peak.load(Ordering::Relaxed) < database_size / 10,
            "peak WAL {} should stay below 10% of database {database_size}",
            peak.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn deletion_sql_uses_fixed_subqueries_without_formatted_in_lists() {
        let source = concat!(
            include_str!("../src/delete/mod.rs"),
            include_str!("../src/delete/sessions.rs")
        );

        assert!(!source.contains("format!"));
        assert!(!source.contains("OFFSET"));
        assert!(source.contains("IN (SELECT id FROM batch_ids)"));
        assert!(source.contains("ORDER BY id LIMIT ?1"));
    }

    #[test]
    fn failed_second_batch_preserves_first_commit_and_integrity() {
        let fixture = fixture(4);
        let database = open_fixture(&fixture);
        database
            .connection()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_second_batch
                 BEFORE DELETE ON session
                 WHEN OLD.id = 'ses_2'
                 BEGIN
                     SELECT RAISE(ABORT, 'forced batch failure');
                 END;",
            )
            .expect("failure trigger should install");

        let error = delete(
            &database,
            &ids(fixture.session_ids.iter().cloned()),
            options(2),
        )
        .expect_err("the second batch should fail");

        assert!(matches!(error, Error::Sqlite { .. }));
        assert_eq!(count(database.connection(), "session"), 2);
        assert_eq!(count(database.connection(), "event_sequence"), 2);
        assert_eq!(count(database.connection(), "event"), 2);
        let remaining: Vec<String> = database
            .connection()
            .prepare("SELECT id FROM session ORDER BY id")
            .expect("remaining sessions should prepare")
            .query_map([], |row| row.get(0))
            .expect("remaining sessions should query")
            .collect::<rusqlite::Result<_>>()
            .expect("remaining sessions should decode");
        assert_eq!(remaining, ["ses_2", "ses_3"]);
        assert_eq!(
            database
                .connection()
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("foreign key check should run"),
            0
        );
    }

    #[test]
    fn batch_time_limit_interrupts_work_with_a_typed_sqlite_error() {
        let fixture = fixture(1);
        let database = open_fixture(&fixture);

        let error = delete(
            &database,
            &ids(fixture.session_ids.iter().cloned()),
            DeleteOptions {
                batch_size: 1,
                batch_time_limit: Duration::ZERO,
            },
        )
        .expect_err("zero time ceiling should be rejected");

        assert!(matches!(error, Error::InvalidArgument { .. }));
    }
}
