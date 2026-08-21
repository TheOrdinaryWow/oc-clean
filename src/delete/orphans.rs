use std::collections::BTreeSet;

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::db::{DatabaseConnection, ReadWrite};
use crate::error::Error;
use crate::select::orphans::{DanglingSessionId, EventAggregateId, RawOrphans};
use crate::select::predicates::SessionIds;
use crate::select::retention;

use super::sessions::{self, DeleteOptions, DeletionReport};

const DEFAULT_MAX_PASSES: usize = 32;
const CREATE_DANGLING_CANDIDATES_SQL: &str = "
    DROP TABLE IF EXISTS dangling_session_candidates;
    CREATE TEMP TABLE dangling_session_candidates(id TEXT PRIMARY KEY) WITHOUT ROWID;
";
const CURRENT_DANGLING_SQL: &str = "
    SELECT child.id
    FROM dangling_session_candidates AS candidate
    CROSS JOIN session AS child ON child.id = candidate.id
    LEFT JOIN session AS parent ON parent.id = child.parent_id
    WHERE child.parent_id IS NOT NULL AND parent.id IS NULL
    ORDER BY child.id
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrphanDeleteOptions {
    pub deletion: DeleteOptions,
    pub keep_recent: u64,
    pub max_passes: usize,
}

impl Default for OrphanDeleteOptions {
    fn default() -> Self {
        Self {
            deletion: DeleteOptions::default(),
            keep_recent: 0,
            max_passes: DEFAULT_MAX_PASSES,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OrphanDeletionReport {
    pub deletion: DeletionReport,
    pub dangling_passes: u64,
    pub iteration_bound_hit: bool,
}

/// Deletes pre-existing database orphans while honoring hard retention.
///
/// # Errors
///
/// Returns a typed error when retention selection or a deletion batch fails.
pub fn delete(
    database: &DatabaseConnection<ReadWrite>,
    raw_orphans: &RawOrphans,
    options: OrphanDeleteOptions,
) -> Result<OrphanDeletionReport, Error> {
    delete_with_progress(database, raw_orphans, options, |_| true)
}

/// Deletes orphan batches and reports every committed batch to the caller.
///
/// # Errors
///
/// Returns the same typed failures as [`delete`].
pub fn delete_with_progress<Progress>(
    database: &DatabaseConnection<ReadWrite>,
    raw_orphans: &RawOrphans,
    options: OrphanDeleteOptions,
    mut batch_committed: Progress,
) -> Result<OrphanDeletionReport, Error>
where
    Progress: FnMut(&DeletionReport) -> bool,
{
    let retained = retention::compute(database, options.keep_recent)?;
    let mut report = OrphanDeletionReport::default();
    let event_aggregate_ids = raw_orphans
        .event_aggregate_ids
        .iter()
        .map(EventAggregateId::as_str)
        .filter(|id| is_session_id(id))
        .map(str::to_owned)
        .collect::<SessionIds>();
    let mut cancelled = false;
    let event_report = sessions::delete_event_aggregates_with_progress(
        database,
        &event_aggregate_ids,
        options.deletion,
        |batch_report| {
            let should_continue = batch_committed(batch_report);
            cancelled = !should_continue;
            should_continue
        },
    )?;
    merge_deletion_report(&mut report.deletion, event_report);
    if cancelled {
        return Ok(report);
    }

    let candidates = raw_orphans
        .dangling_session_ids
        .iter()
        .map(DanglingSessionId::as_str)
        .filter(|id| !retained.contains(*id))
        .map(str::to_owned)
        .collect::<SessionIds>();
    let candidates = DanglingCandidates::materialize(database.connection(), &candidates)?;

    for _ in 0..options.max_passes {
        let dangling = candidates.current()?;
        if dangling.is_empty() {
            return Ok(report);
        }
        merge_deletion_report(
            &mut report.deletion,
            sessions::delete_with_progress(
                database,
                &dangling,
                options.deletion,
                &mut batch_committed,
            )?,
        );
        candidates.remove(&dangling)?;
        report.dangling_passes = report.dangling_passes.saturating_add(1);
    }

    report.iteration_bound_hit = !candidates.current()?.is_empty();
    Ok(report)
}

fn is_session_id(value: &str) -> bool {
    value.strip_prefix("ses_").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

struct DanglingCandidates<'connection> {
    connection: &'connection Connection,
}

impl<'connection> DanglingCandidates<'connection> {
    fn materialize(
        connection: &'connection Connection,
        candidates: &SessionIds,
    ) -> Result<Self, Error> {
        connection
            .execute_batch(CREATE_DANGLING_CANDIDATES_SQL)
            .map_err(|source| sqlite_error("creating dangling-session candidate table", source))?;
        let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Deferred)
            .map_err(|source| sqlite_error("starting dangling-session materialization", source))?;
        {
            let mut insert = transaction
                .prepare("INSERT INTO dangling_session_candidates(id) VALUES (?1)")
                .map_err(|source| {
                    sqlite_error("preparing dangling-session materialization", source)
                })?;
            for candidate in candidates {
                insert.execute([candidate]).map_err(|source| {
                    sqlite_error("materializing dangling-session candidates", source)
                })?;
            }
        }
        transaction.commit().map_err(|source| {
            sqlite_error("committing dangling-session materialization", source)
        })?;
        Ok(Self { connection })
    }

    fn current(&self) -> Result<SessionIds, Error> {
        let mut statement = self
            .connection
            .prepare(CURRENT_DANGLING_SQL)
            .map_err(|source| sqlite_error("preparing dangling-session deletion pass", source))?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|source| sqlite_error("querying dangling-session deletion pass", source))?
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(|source| sqlite_error("reading dangling-session deletion pass", source))
    }

    fn remove(&self, candidates: &SessionIds) -> Result<(), Error> {
        let transaction =
            Transaction::new_unchecked(self.connection, TransactionBehavior::Deferred).map_err(
                |source| sqlite_error("starting dangling-session candidate removal", source),
            )?;
        {
            let mut remove = transaction
                .prepare("DELETE FROM dangling_session_candidates WHERE id = ?1")
                .map_err(|source| {
                    sqlite_error("preparing dangling-session candidate removal", source)
                })?;
            for candidate in candidates {
                remove.execute([candidate]).map_err(|source| {
                    sqlite_error("removing dangling-session candidates", source)
                })?;
            }
        }
        transaction
            .commit()
            .map_err(|source| sqlite_error("committing dangling-session candidate removal", source))
    }
}

fn merge_deletion_report(target: &mut DeletionReport, source: DeletionReport) {
    for (table, rows) in source.table_rows {
        target
            .table_rows
            .entry(table)
            .and_modify(|total| *total = total.saturating_add(rows))
            .or_insert(rows);
    }
    target.transactions = target.transactions.saturating_add(source.transactions);
    target
        .deleted_session_ids
        .extend(source.deleted_session_ids);
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        context: context.to_owned(),
        source,
    }
}

#[cfg(test)]
#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use rusqlite::{params, Connection};

    use super::fixture::{Fixture, FixtureConfig, BASE_TIME_MS, TABLES as ALL_TABLES};
    use super::*;
    use crate::db::{open_read_write, ConnectionOptions, ReadWriteConnection};
    use crate::paths::{derived_paths, Target};
    use crate::select::orphans;

    const SESSION_TABLES: &[&str] = &["session", "event_sequence", "event"];

    fn open_fixture(fixture: &Fixture) -> ReadWriteConnection {
        open_read_write(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-write")
    }

    fn options(keep_recent: u64, max_passes: usize) -> OrphanDeleteOptions {
        OrphanDeleteOptions {
            deletion: DeleteOptions {
                batch_size: 2,
                batch_time_limit: Duration::from_secs(30),
            },
            keep_recent,
            max_passes,
        }
    }

    fn raw(fixture: &Fixture, database: &ReadWriteConnection) -> RawOrphans {
        orphans::select(database, &derived_paths(fixture.root()))
            .expect("raw orphan selection should succeed")
    }

    fn insert_session(connection: &Connection, id: &str, parent_id: &str, updated: i64) {
        connection
            .execute(
                "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated) VALUES (?1, 'project-0', ?2, ?1, '/fixture/project-0', ?1, '1.18.19', ?3, ?3)",
                params![id, parent_id, updated],
            )
            .expect("dangling session should insert");
        connection
            .execute("INSERT INTO event_sequence VALUES (?1, 1, NULL)", [id])
            .expect("session event sequence should insert");
        connection
            .execute(
                "INSERT INTO event VALUES (?1, ?2, 1, 'session.created', '{}')",
                params![format!("event-{id}"), id],
            )
            .expect("session event should insert");
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

    fn counts(connection: &Connection) -> BTreeMap<&'static str, i64> {
        SESSION_TABLES
            .iter()
            .map(|table| {
                let count = connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .expect("table should count");
                (*table, count)
            })
            .collect()
    }

    fn all_counts(connection: &Connection) -> BTreeMap<&'static str, i64> {
        ALL_TABLES
            .iter()
            .map(|table| {
                let count = connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .expect("fixture table should count");
                (*table, count)
            })
            .collect()
    }

    #[test]
    fn removes_three_deep_dangling_chain_in_one_invocation() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let setup = fixture.connect().expect("fixture should connect");
        insert_session(&setup, "ses_DanglingA", "ses_Gone", BASE_TIME_MS - 3);
        insert_session(&setup, "ses_DanglingB", "ses_DanglingA", BASE_TIME_MS - 2);
        insert_session(&setup, "ses_DanglingC", "ses_DanglingB", BASE_TIME_MS - 1);
        drop(setup);
        let database = open_fixture(&fixture);
        let raw = raw(&fixture, &database);

        let report = delete(&database, &raw, options(0, 32)).expect("sweep should succeed");

        assert_eq!(report.deletion.table_rows["session"], 3);
        assert_eq!(report.dangling_passes, 3);
        assert!(!report.iteration_bound_hit);
        assert_eq!(counts(database.connection())["session"], 1);
    }

    #[test]
    fn removes_exactly_seven_orphan_events_and_three_dangling_sessions() {
        let fixture = Fixture::build(&FixtureConfig {
            dangling_parent_session_count: 3,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        insert_orphan_events(&fixture.connect().expect("fixture should connect"), 7);
        let database = open_fixture(&fixture);
        let before = counts(database.connection());
        let raw = raw(&fixture, &database);

        let report = delete(&database, &raw, options(0, 32)).expect("sweep should succeed");
        let after = counts(database.connection());

        assert_eq!(before["session"] - after["session"], 3);
        assert_eq!(before["event_sequence"] - after["event_sequence"], 10);
        assert_eq!(before["event"] - after["event"], 10);
        assert_eq!(report.deletion.table_rows["session"], 3);
        assert_eq!(report.deletion.table_rows["event_sequence"], 10);
        assert_eq!(report.deletion.table_rows["event"], 10);
        assert_eq!(report.deletion.deleted_session_ids.len(), 3);
        assert!(report
            .deletion
            .deleted_session_ids
            .iter()
            .all(|id| !id.starts_with("ses_Orphan")));
    }

    #[test]
    fn preserves_live_sessions_and_their_events() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let database = open_fixture(&fixture);
        let before = all_counts(database.connection());
        let raw = raw(&fixture, &database);

        let report = delete(&database, &raw, options(0, 32)).expect("sweep should succeed");

        assert_eq!(all_counts(database.connection()), before);
        assert_eq!(report, OrphanDeletionReport::default());
    }

    #[test]
    fn preserves_non_session_shaped_orphan_event_aggregates() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let setup = fixture.connect().expect("fixture should connect");
        for aggregate_id in ["ses_has_underscore", "ses_nonasciié", "prj_something"] {
            setup
                .execute(
                    "INSERT INTO event_sequence VALUES (?1, 1, NULL)",
                    [aggregate_id],
                )
                .expect("non-session sequence should insert");
        }
        drop(setup);
        let database = open_fixture(&fixture);
        let before = counts(database.connection());
        let raw = raw(&fixture, &database);

        let report = delete(&database, &raw, options(0, 32)).expect("sweep should succeed");

        assert_eq!(counts(database.connection()), before);
        assert_eq!(report, OrphanDeletionReport::default());
    }

    #[test]
    fn cancellation_after_event_aggregate_batch_skips_dangling_sessions() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let setup = fixture.connect().expect("fixture should connect");
        insert_orphan_events(&setup, 3);
        insert_session(
            &setup,
            "ses_DanglingAfterCancel",
            "ses_Gone",
            BASE_TIME_MS - 1,
        );
        drop(setup);
        let database = open_fixture(&fixture);
        let raw = raw(&fixture, &database);

        let report = delete_with_progress(&database, &raw, options(0, 32), |_| false)
            .expect("cancelled sweep should preserve committed work");

        assert_eq!(report.deletion.transactions, 1);
        assert_eq!(report.deletion.table_rows["event_sequence"], 2);
        assert_eq!(report.dangling_passes, 0);
        assert!(database
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM session WHERE id = 'ses_DanglingAfterCancel')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("dangling session existence should query"));
    }

    #[test]
    fn retention_protects_a_dangling_root_and_its_descendant() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let setup = fixture.connect().expect("fixture should connect");
        insert_session(&setup, "ses_KeptRoot", "ses_Gone", BASE_TIME_MS + 2);
        insert_session(&setup, "ses_KeptChild", "ses_KeptRoot", BASE_TIME_MS + 3);
        drop(setup);
        let database = open_fixture(&fixture);
        let raw = raw(&fixture, &database);

        let report = delete(&database, &raw, options(1, 32)).expect("sweep should succeed");

        assert_eq!(report, OrphanDeletionReport::default());
        assert_eq!(counts(database.connection())["session"], 3);
    }

    #[test]
    fn enforces_and_reports_iteration_bound() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let setup = fixture.connect().expect("fixture should connect");
        insert_session(&setup, "ses_DanglingA", "ses_Gone", BASE_TIME_MS - 3);
        insert_session(&setup, "ses_DanglingB", "ses_DanglingA", BASE_TIME_MS - 2);
        insert_session(&setup, "ses_DanglingC", "ses_DanglingB", BASE_TIME_MS - 1);
        drop(setup);
        let database = open_fixture(&fixture);
        let raw = raw(&fixture, &database);

        let report = delete(&database, &raw, options(0, 2)).expect("bounded sweep should report");

        assert_eq!(report.deletion.table_rows["session"], 2);
        assert_eq!(report.dangling_passes, 2);
        assert!(report.iteration_bound_hit);
        assert_eq!(counts(database.connection())["session"], 2);
    }

    #[test]
    fn dangling_pass_query_uses_temp_candidates_without_scanning_sessions() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 3_000,
            ..FixtureConfig::default()
        })
        .expect("query-plan fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute_batch(CREATE_DANGLING_CANDIDATES_SQL)
            .expect("candidate table should materialize");
        connection
            .execute(
                "INSERT INTO dangling_session_candidates SELECT id FROM session ORDER BY id LIMIT 10",
                [],
            )
            .expect("candidate ids should insert");

        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {CURRENT_DANGLING_SQL}"))
            .expect("query plan should prepare");
        let details = statement
            .query_map([], |row| row.get::<_, String>(3))
            .expect("query plan should execute")
            .collect::<Result<Vec<_>, _>>()
            .expect("query plan should read");
        let normalized = details
            .iter()
            .map(|detail| detail.to_ascii_lowercase())
            .collect::<Vec<_>>();

        assert!(normalized
            .iter()
            .any(|detail| detail.starts_with("scan candidate")));
        assert!(normalized.iter().any(|detail| {
            detail.contains("search child") && detail.contains("sqlite_autoindex_session_1")
        }));
        assert!(normalized
            .iter()
            .all(|detail| !detail.starts_with("scan child")));
    }
}
