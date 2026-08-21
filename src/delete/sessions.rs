use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction};

use super::TempIdBatcher;
use crate::db::{DatabaseConnection, ReadWrite};
use crate::error::Error;
use crate::select::predicates::SessionIds;

const DEFAULT_BATCH_SIZE: usize = 5_000;
const DEFAULT_BATCH_TIME_LIMIT: Duration = Duration::from_secs(30);
const PROGRESS_HANDLER_OPS: i32 = 1_000;
const DELETE_SESSION_SQL: &str = "DELETE FROM session WHERE id IN (SELECT id FROM batch_ids)";
const DELETE_EVENT_SEQUENCE_SQL: &str =
    "DELETE FROM event_sequence WHERE aggregate_id IN (SELECT id FROM batch_ids)";

const TABLE_COUNTS: &[TableCount] = &[
    TableCount::new(
        "event",
        "SELECT COUNT(*) FROM event WHERE aggregate_id IN (SELECT id FROM batch_ids)",
    ),
    TableCount::new(
        "event_sequence",
        "SELECT COUNT(*) FROM event_sequence WHERE aggregate_id IN (SELECT id FROM batch_ids)",
    ),
    TableCount::new(
        "message",
        "SELECT COUNT(*) FROM message WHERE session_id IN (SELECT id FROM batch_ids)",
    ),
    TableCount::new(
        "part",
        "SELECT COUNT(*) FROM part WHERE session_id IN (SELECT id FROM batch_ids)",
    ),
    TableCount::new(
        "session",
        "SELECT COUNT(*) FROM session WHERE id IN (SELECT id FROM batch_ids)",
    ),
    TableCount::new(
        "session_context_epoch",
        "SELECT COUNT(*) FROM session_context_epoch WHERE session_id IN (SELECT id FROM batch_ids)",
    ),
    TableCount::new(
        "session_input",
        "SELECT COUNT(*) FROM session_input WHERE session_id IN (SELECT id FROM batch_ids)",
    ),
    TableCount::new(
        "session_message",
        "SELECT COUNT(*) FROM session_message WHERE session_id IN (SELECT id FROM batch_ids)",
    ),
    TableCount::new(
        "session_share",
        "SELECT COUNT(*) FROM session_share WHERE session_id IN (SELECT id FROM batch_ids)",
    ),
    TableCount::new(
        "todo",
        "SELECT COUNT(*) FROM todo WHERE session_id IN (SELECT id FROM batch_ids)",
    ),
];

#[derive(Clone, Copy)]
struct TableCount {
    table: &'static str,
    sql: &'static str,
}

impl TableCount {
    const fn new(table: &'static str, sql: &'static str) -> Self {
        Self { table, sql }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeleteOptions {
    pub batch_size: usize,
    pub batch_time_limit: Duration,
}

impl Default for DeleteOptions {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            batch_time_limit: DEFAULT_BATCH_TIME_LIMIT,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionReport {
    pub table_rows: BTreeMap<String, u64>,
    pub transactions: u64,
}

impl Default for DeletionReport {
    fn default() -> Self {
        Self {
            table_rows: TABLE_COUNTS
                .iter()
                .map(|table| (table.table.to_owned(), 0))
                .collect(),
            transactions: 0,
        }
    }
}

struct ProgressDeadline<'connection> {
    connection: &'connection Connection,
    deadline: Instant,
}

impl<'connection> ProgressDeadline<'connection> {
    fn install(connection: &'connection Connection, duration: Duration) -> Result<Self, Error> {
        let deadline =
            Instant::now()
                .checked_add(duration)
                .ok_or_else(|| Error::InvalidArgument {
                    argument: "batch_time_limit".to_owned(),
                    reason: "duration exceeds the monotonic clock range".to_owned(),
                })?;
        connection
            .progress_handler(
                PROGRESS_HANDLER_OPS,
                Some(move || Instant::now() >= deadline),
            )
            .map_err(|source| sqlite_error("installing deletion batch time ceiling", source))?;
        Ok(Self {
            connection,
            deadline,
        })
    }

    fn ensure_remaining(&self) -> Result<(), Error> {
        if Instant::now() >= self.deadline {
            return Err(batch_deadline_error());
        }
        Ok(())
    }
}

impl Drop for ProgressDeadline<'_> {
    fn drop(&mut self) {
        let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
    }
}

/// Deletes the finalized set of sessions in bounded transactions.
///
/// # Errors
///
/// Returns a typed error when foreign keys are disabled, options are invalid, or SQLite cannot
/// materialize or delete the selected sessions.
pub fn delete(
    database: &DatabaseConnection<ReadWrite>,
    session_ids: &SessionIds,
    options: DeleteOptions,
) -> Result<DeletionReport, Error> {
    let batch_size = validate_options(options)?;
    ensure_foreign_keys(database.connection())?;
    let batcher = TempIdBatcher::materialize(
        database.connection(),
        session_ids.iter().map(String::as_str),
    )?;
    let mut report = DeletionReport::default();

    loop {
        let deadline = ProgressDeadline::install(database.connection(), options.batch_time_limit)?;
        let Some(transaction) = batcher.begin_batch(batch_size)? else {
            break;
        };
        deadline.ensure_remaining()?;
        accumulate_counts(&transaction, &mut report)?;
        deadline.ensure_remaining()?;
        transaction
            .execute(DELETE_SESSION_SQL, [])
            .map_err(|source| sqlite_error("deleting a session batch", source))?;
        deadline.ensure_remaining()?;
        transaction
            .execute(DELETE_EVENT_SEQUENCE_SQL, [])
            .map_err(|source| sqlite_error("deleting session event aggregates", source))?;
        TempIdBatcher::remove_completed(&transaction)?;
        deadline.ensure_remaining()?;
        transaction
            .commit()
            .map_err(|source| sqlite_error("committing a session deletion batch", source))?;
        drop(deadline);
        checkpoint_wal(database.connection())?;
        report.transactions = report.transactions.saturating_add(1);
    }

    Ok(report)
}

fn validate_options(options: DeleteOptions) -> Result<i64, Error> {
    if options.batch_size == 0 {
        return Err(Error::InvalidArgument {
            argument: "batch_size".to_owned(),
            reason: "must be greater than zero".to_owned(),
        });
    }
    if options.batch_time_limit.is_zero() {
        return Err(Error::InvalidArgument {
            argument: "batch_time_limit".to_owned(),
            reason: "must be greater than zero".to_owned(),
        });
    }
    i64::try_from(options.batch_size).map_err(|_| Error::InvalidArgument {
        argument: "batch_size".to_owned(),
        reason: "exceeds SQLite's signed integer range".to_owned(),
    })
}

fn ensure_foreign_keys(connection: &Connection) -> Result<(), Error> {
    let enabled = connection
        .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
        .map_err(|source| sqlite_error("verifying PRAGMA foreign_keys", source))?;
    if enabled != 1 {
        return Err(Error::IntegrityCheckFailed {
            check: "PRAGMA foreign_keys".to_owned(),
            message: "session deletion requires foreign_keys=1".to_owned(),
        });
    }
    Ok(())
}

fn accumulate_counts(
    transaction: &Transaction<'_>,
    report: &mut DeletionReport,
) -> Result<(), Error> {
    for table in TABLE_COUNTS {
        let count = transaction
            .query_row(table.sql, [], |row| row.get::<_, i64>(0))
            .map_err(|source| sqlite_error("counting rows in a session deletion batch", source))?;
        let count = u64::try_from(count).map_err(|_| {
            sqlite_error(
                "decoding a session deletion row count",
                rusqlite::Error::IntegralValueOutOfRange(0, count),
            )
        })?;
        report
            .table_rows
            .entry(table.table.to_owned())
            .and_modify(|total| *total = total.saturating_add(count));
    }
    Ok(())
}

fn checkpoint_wal(connection: &Connection) -> Result<(), Error> {
    let busy = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|source| sqlite_error("checkpointing the WAL after a deletion batch", source))?;
    if busy != 0 {
        return Err(Error::DatabaseBusy {
            holders: vec!["WAL checkpoint could not truncate all frames".to_owned()],
        });
    }
    Ok(())
}

fn batch_deadline_error() -> Error {
    Error::Sqlite {
        context: "enforcing the session deletion batch time ceiling".to_owned(),
        source: rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
            Some("session deletion batch exceeded its wall-clock ceiling".to_owned()),
        ),
    }
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        context: context.to_owned(),
        source,
    }
}
