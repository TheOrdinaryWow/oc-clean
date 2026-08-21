use thiserror::Error as ThisError;

use crate::db::{DatabaseConnection, ReadWriteConnection};

/// Default maximum number of freelist pages requested from SQLite per batch.
pub const DEFAULT_PAGES_PER_BATCH: u32 = 256;

/// Cumulative work completed by the incremental vacuum loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncrementalVacuumProgress {
    pub pages_reclaimed: u64,
    pub bytes_reclaimed: u64,
}

/// Final page and byte totals reclaimed by a completed incremental vacuum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncrementalVacuumReport {
    pub pages_reclaimed: u64,
    pub bytes_reclaimed: u64,
}

impl From<IncrementalVacuumReport> for IncrementalVacuumProgress {
    fn from(report: IncrementalVacuumReport) -> Self {
        Self {
            pages_reclaimed: report.pages_reclaimed,
            bytes_reclaimed: report.bytes_reclaimed,
        }
    }
}

/// Failures specific to incremental vacuum preconditions and execution.
#[derive(Debug, ThisError)]
pub enum IncrementalVacuumError {
    #[error(
        "incremental vacuum requires PRAGMA auto_vacuum=INCREMENTAL (2), found {actual_mode}; SQLite can only change auto_vacuum on an existing non-empty database via a full VACUUM, so the full VACUUM rebuild path is required"
    )]
    AutoVacuumNotIncremental { actual_mode: u32 },
    #[error("incremental vacuum pages per batch must be greater than zero")]
    InvalidBatchSize,
    #[error(
        "incremental vacuum was cancelled after reclaiming {pages} pages ({bytes} bytes)",
        pages = progress.pages_reclaimed,
        bytes = progress.bytes_reclaimed
    )]
    Cancelled { progress: IncrementalVacuumProgress },
    #[error("SQLite operation failed while {context}: {source}")]
    Sqlite {
        context: &'static str,
        #[source]
        source: rusqlite::Error,
    },
}

/// Verifies that the database was previously configured for incremental auto-vacuum.
///
/// # Errors
///
/// Returns [`IncrementalVacuumError::AutoVacuumNotIncremental`] when the existing database does
/// not already report mode `2`, or [`IncrementalVacuumError::Sqlite`] when SQLite cannot read the
/// pragma.
pub fn check_auto_vacuum<Access>(
    database: &DatabaseConnection<Access>,
) -> Result<(), IncrementalVacuumError> {
    let actual_mode = database
        .connection()
        .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
        .map_err(|source| sqlite_error("reading PRAGMA auto_vacuum", source))?;
    if actual_mode == 2 {
        Ok(())
    } else {
        Err(IncrementalVacuumError::AutoVacuumNotIncremental { actual_mode })
    }
}

/// Reclaims freelist pages in bounded SQLite calls while remaining interruptible between calls.
///
/// `cancelled` is checked before every batch, and `report_progress` receives cumulative page and
/// byte totals after every completed batch.
///
/// # Errors
///
/// Returns a typed precondition, batch-size, cancellation, or SQLite error. Cancellation contains
/// the cumulative progress completed before the signal was observed.
pub fn incremental_vacuum<Cancelled, Progress>(
    database: &ReadWriteConnection,
    pages_per_batch: u32,
    mut cancelled: Cancelled,
    mut report_progress: Progress,
) -> Result<IncrementalVacuumReport, IncrementalVacuumError>
where
    Cancelled: FnMut() -> bool,
    Progress: FnMut(IncrementalVacuumProgress),
{
    check_auto_vacuum(database)?;
    if pages_per_batch == 0 {
        return Err(IncrementalVacuumError::InvalidBatchSize);
    }

    let connection = database.connection();
    let page_size = pragma_u64(connection, "page_size")?;
    let initial_freelist = pragma_u64(connection, "freelist_count")?;
    let mut remaining = initial_freelist;
    let mut progress = IncrementalVacuumProgress {
        pages_reclaimed: 0,
        bytes_reclaimed: 0,
    };
    let statement = format!("PRAGMA incremental_vacuum({pages_per_batch})");

    while remaining > 0 {
        if cancelled() {
            return Err(IncrementalVacuumError::Cancelled { progress });
        }
        connection
            .execute_batch(&statement)
            .map_err(|source| sqlite_error("running a bounded incremental vacuum batch", source))?;
        let current = pragma_u64(connection, "freelist_count")?;
        progress.pages_reclaimed = initial_freelist.saturating_sub(current);
        progress.bytes_reclaimed = progress.pages_reclaimed.saturating_mul(page_size);
        report_progress(progress);
        if current >= remaining {
            break;
        }
        remaining = current;
    }

    Ok(IncrementalVacuumReport {
        pages_reclaimed: progress.pages_reclaimed,
        bytes_reclaimed: progress.bytes_reclaimed,
    })
}

fn pragma_u64(
    connection: &rusqlite::Connection,
    pragma: &'static str,
) -> Result<u64, IncrementalVacuumError> {
    let value = connection
        .pragma_query_value(None, pragma, |row| row.get::<_, i64>(0))
        .map_err(|source| sqlite_error("reading incremental vacuum accounting pragma", source))?;
    u64::try_from(value).map_err(|_| {
        sqlite_error(
            "reading a non-negative incremental vacuum accounting value",
            rusqlite::Error::IntegralValueOutOfRange(0, value),
        )
    })
}

fn sqlite_error(context: &'static str, source: rusqlite::Error) -> IncrementalVacuumError {
    IncrementalVacuumError::Sqlite { context, source }
}

#[cfg(test)]
#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;

    use rusqlite::Connection;

    use super::fixture::{Fixture, FixtureConfig};
    use super::*;
    use crate::db::{ConnectionOptions, open_read_write};
    use crate::paths::Target;

    const BLOAT_BYTES_PER_PART: usize = 64 * 1_024;

    fn fixture() -> Fixture {
        Fixture::build(&FixtureConfig {
            session_count: 12,
            messages_per_session: 2,
            parts_per_message: 2,
            blob_size_per_part: BLOAT_BYTES_PER_PART,
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

    fn pragma_u64(connection: &Connection, pragma: &str) -> u64 {
        connection
            .pragma_query_value(None, pragma, |row| row.get::<_, i64>(0))
            .and_then(|value| {
                u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
            })
            .expect("pragma should contain a non-negative integer")
    }

    fn prepare_incremental_bloat(fixture: &Fixture) -> (ReadWriteConnection, u64, u64) {
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .expect("auto_vacuum should be requested");
        connection
            .execute_batch("VACUUM; DELETE FROM session;")
            .expect("full vacuum should set the mode before deletion creates bloat");
        let freelist = pragma_u64(&connection, "freelist_count");
        assert!(freelist > 1, "fixture should contain multiple free pages");
        drop(connection);
        let bytes = fs::metadata(&fixture.database_path)
            .expect("fixture metadata should exist")
            .len();
        (open_fixture(fixture), freelist, bytes)
    }

    #[test]
    fn incremental_mode_reduces_freelist_and_shrinks_file() {
        let fixture = fixture();
        let (database, freelist_before, bytes_before) = prepare_incremental_bloat(&fixture);
        let mut progress = Vec::new();

        let report = incremental_vacuum(
            &database,
            DEFAULT_PAGES_PER_BATCH,
            || false,
            |update| progress.push(update),
        )
        .expect("incremental vacuum should complete");

        let freelist_after = pragma_u64(database.connection(), "freelist_count");
        drop(database);
        let bytes_after = fs::metadata(&fixture.database_path)
            .expect("fixture metadata should exist")
            .len();
        assert!(freelist_after < freelist_before);
        assert!(bytes_after < bytes_before);
        assert_eq!(report.pages_reclaimed, freelist_before - freelist_after);
        assert_eq!(progress.last().copied(), Some(report.into()));
    }

    #[test]
    fn none_mode_measurement_reclaims_zero_and_typed_check_preserves_bytes() {
        let fixture = fixture();
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute("DELETE FROM session", [])
            .expect("fixture rows should delete");
        let freelist_before = pragma_u64(&connection, "freelist_count");
        assert!(freelist_before > 0, "fixture should contain free pages");
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .expect("SQLite should accept the pragma statement");
        assert_eq!(pragma_u64(&connection, "auto_vacuum"), 0);
        connection
            .execute_batch("PRAGMA incremental_vacuum(100)")
            .expect("incremental vacuum pragma should execute");
        assert_eq!(pragma_u64(&connection, "freelist_count"), freelist_before);
        drop(connection);

        let bytes_before = fs::read(&fixture.database_path).expect("fixture bytes should read");
        let database = open_fixture(&fixture);
        let error = check_auto_vacuum(&database).expect_err("NONE mode should be refused");
        assert!(matches!(
            error,
            IncrementalVacuumError::AutoVacuumNotIncremental { actual_mode: 0 }
        ));
        assert!(error.to_string().contains("full VACUUM"));
        drop(database);
        let bytes_after = fs::read(&fixture.database_path).expect("fixture bytes should read");
        assert_eq!(bytes_after, bytes_before);
    }

    #[test]
    fn cancellation_stops_after_one_bounded_batch() {
        let fixture = fixture();
        let (database, freelist_before, _) = prepare_incremental_bloat(&fixture);
        let cancellation_checks = Cell::new(0_u32);
        let mut progress = Vec::new();

        let error = incremental_vacuum(
            &database,
            1,
            || {
                let checks = cancellation_checks.get();
                cancellation_checks.set(checks + 1);
                checks > 0
            },
            |update| progress.push(update),
        )
        .expect_err("second cancellation check should stop the loop");

        let freelist_after = pragma_u64(database.connection(), "freelist_count");
        let expected = IncrementalVacuumProgress {
            pages_reclaimed: freelist_before - freelist_after,
            bytes_reclaimed: (freelist_before - freelist_after)
                * pragma_u64(database.connection(), "page_size"),
        };
        assert_eq!(expected.pages_reclaimed, 1);
        assert!(matches!(
            error,
            IncrementalVacuumError::Cancelled { progress } if progress == expected
        ));
        assert_eq!(progress, vec![expected]);
        assert!(freelist_after > 0, "one batch must leave remaining pages");
    }
}
