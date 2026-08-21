use std::path::Path;

use crate::analyze::space::{self, FileSpace};
use crate::cli::{CleanArgs, Cli};
use crate::db::{self, ReadWriteConnection};
use crate::delete::sessions::DEFAULT_BATCH_SIZE;
use crate::error::Error;
use crate::reclaim::headroom::{
    FreeSpaceProvider, HeadroomInput, HeadroomVerdict, evaluate_headroom,
};
use crate::reclaim::incremental::{IncrementalVacuumError, incremental_vacuum};
use crate::reclaim::vacuum_into::{VacuumIntoOptions, vacuum_into_with_observer};

use super::signal::SignalController;

const INCREMENTAL_PAGES_PER_BATCH: u32 = 1_000;
const WAL_DIRTY_PAGES_PER_SESSION: u64 = 64;
const WAL_HEADER_BYTES: u64 = 32;
const WAL_FRAME_HEADER_BYTES: u64 = 24;

pub(super) fn pre_delete_headroom(
    database_path: &Path,
    file: &FileSpace,
    current_live_bytes: u64,
    selected_session_bytes: u64,
    selected_session_count: usize,
    hard_link_available: bool,
    free_space: &impl FreeSpaceProvider,
) -> Result<(), Error> {
    ensure_headroom(
        database_path,
        current_live_bytes,
        selected_session_bytes,
        file.total_bytes,
        one_batch_wal_allowance(
            selected_session_bytes,
            selected_session_count,
            file.page_size,
        ),
        hard_link_available,
        free_space,
    )
}

pub(super) fn post_delete_headroom(
    database: &ReadWriteConnection,
    database_path: &Path,
    cli: &Cli,
    arguments: &CleanArgs,
    free_space: &impl FreeSpaceProvider,
) -> Result<(), Error> {
    if arguments.incremental || arguments.no_vacuum {
        return Ok(());
    }
    let file = space::analyze(database)?.file;
    ensure_headroom(
        database_path,
        file.live_bytes,
        0,
        file.total_bytes,
        u64::from(file.page_size),
        database.capabilities().hard_links || cli.skip_backup,
        free_space,
    )
}

pub(super) fn run(
    database: ReadWriteConnection,
    database_path: &Path,
    locked_data_version: i64,
    cli: &Cli,
    arguments: &CleanArgs,
    signals: &SignalController,
) -> Result<u64, Error> {
    if arguments.no_vacuum {
        return Ok(0);
    }
    signals.begin_reclaim();
    if arguments.incremental {
        let result = incremental_vacuum(
            &database,
            INCREMENTAL_PAGES_PER_BATCH,
            || signals.cancelled(),
            |_| {},
        );
        return match result {
            Ok(report) => Ok(report.bytes_reclaimed),
            Err(_error) if signals.cancelled() => Err(interrupted("incremental vacuum")),
            Err(error) => Err(incremental_error(error)),
        };
    }
    let result = vacuum_into_with_observer(
        database,
        database_path,
        locked_data_version,
        VacuumIntoOptions {
            skip_backup: cli.skip_backup,
        },
        signals,
    );
    match result {
        Ok(_report) if signals.cancelled() => Err(interrupted("atomic database swap")),
        Ok(report) => Ok(report.bytes_reclaimed),
        Err(_) if signals.cancelled() => Err(interrupted("VACUUM output cleanup")),
        Err(error) => Err(error),
    }
}

fn ensure_headroom(
    database_path: &Path,
    current_live_bytes: u64,
    selected_session_bytes: u64,
    full_original_size: u64,
    one_batch_wal_allowance: u64,
    hardlink_supported: bool,
    free_space: &impl FreeSpaceProvider,
) -> Result<(), Error> {
    let estimate = evaluate_headroom(
        free_space,
        database_path,
        HeadroomInput {
            current_live_bytes,
            selected_session_bytes,
            full_original_size,
            one_batch_wal_allowance,
            margin_fraction: HeadroomInput::DEFAULT_MARGIN_FRACTION,
            hardlink_supported,
        },
    )
    .map_err(|source| Error::Io {
        path: database_path.to_owned(),
        source,
    })?;
    match estimate.verdict {
        HeadroomVerdict::Sufficient => Ok(()),
        HeadroomVerdict::InsufficientWithShortfall { .. } => Err(Error::InsufficientDiskSpace {
            required_bytes: estimate.required_bytes,
            available_bytes: estimate.available_bytes,
        }),
    }
}

/// Conservatively bounds the WAL space needed by one delete batch.
///
/// The current cascade can touch roughly 36 table and index b-trees per session. Reserving 64 frames
/// per candidate leaves room for interior pages, freelist maintenance, and page splits. The complete
/// selected payload remains in the estimate because only an aggregate is available here and an
/// ID-sorted batch can contain every large candidate. This intentionally favors an early headroom
/// refusal over exhausting the filesystem before the post-batch checkpoint.
fn one_batch_wal_allowance(selected_bytes: u64, selected_count: usize, page_size: u32) -> u64 {
    let batch_count = selected_count.min(DEFAULT_BATCH_SIZE);
    let batch_count = u64::try_from(batch_count).unwrap_or(u64::MAX);
    let frame_bytes = u64::from(page_size).saturating_add(WAL_FRAME_HEADER_BYTES);
    let structural_bytes = batch_count
        .saturating_mul(WAL_DIRTY_PAGES_PER_SESSION)
        .saturating_mul(frame_bytes);

    WAL_HEADER_BYTES
        .saturating_add(frame_bytes)
        .saturating_add(selected_bytes)
        .saturating_add(structural_bytes)
}

fn incremental_error(error: IncrementalVacuumError) -> Error {
    match error {
        IncrementalVacuumError::Sqlite { context, source } => db::sqlite_error(context, source),
        IncrementalVacuumError::Cancelled { progress } => Error::Interrupted {
            completed: format!(
                "{} pages and {} bytes reclaimed",
                progress.pages_reclaimed, progress.bytes_reclaimed
            ),
        },
        other => Error::ReclaimUnavailable {
            reason: other.to_string(),
        },
    }
}

fn interrupted(completed: &str) -> Error {
    Error::Interrupted {
        completed: completed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_allowance_covers_all_selected_candidate_bytes() {
        let selected_count = DEFAULT_BATCH_SIZE.saturating_mul(2);
        let selected_bytes = u64::try_from(selected_count)
            .expect("test count should fit u64")
            .saturating_mul(10);
        assert!(
            one_batch_wal_allowance(selected_bytes, selected_count, 4_096)
                > selected_bytes.saturating_add(4_096)
        );
    }

    #[test]
    fn wal_allowance_bounds_a_skewed_id_sorted_batch() {
        let mut candidate_bytes = vec![1_u64; DEFAULT_BATCH_SIZE.saturating_mul(2)];
        candidate_bytes[DEFAULT_BATCH_SIZE..].fill(1_000);
        let selected_bytes = candidate_bytes.iter().copied().sum::<u64>();
        let actual_peak = candidate_bytes
            .chunks(DEFAULT_BATCH_SIZE)
            .map(|batch| batch.iter().copied().sum::<u64>())
            .max()
            .expect("test fixture should contain candidates")
            .saturating_add(4_096);

        assert!(
            one_batch_wal_allowance(selected_bytes, candidate_bytes.len(), 4_096) >= actual_peak
        );
    }

    #[test]
    fn wal_allowance_covers_dirty_pages_for_many_zero_payload_sessions() {
        let selected_count = DEFAULT_BATCH_SIZE;
        let page_size = 4_096_u32;
        let reasonable_dirty_page_floor = u64::try_from(selected_count)
            .expect("test count should fit u64")
            .saturating_mul(4)
            .saturating_mul(u64::from(page_size));

        assert!(
            one_batch_wal_allowance(0, selected_count, page_size) >= reasonable_dirty_page_floor
        );
    }

    #[test]
    fn incremental_busy_error_uses_database_busy_category() {
        let source = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );

        assert!(matches!(
            incremental_error(IncrementalVacuumError::Sqlite {
                context: "incremental vacuum",
                source,
            }),
            Error::DatabaseBusy { .. }
        ));
    }
}
