use std::path::Path;

use crate::analyze::space::{self, FileSpace};
use crate::cli::{CleanArgs, Cli};
use crate::db::ReadWriteConnection;
use crate::error::Error;
use crate::reclaim::headroom::{
    FreeSpaceProvider, HeadroomInput, HeadroomVerdict, evaluate_headroom,
};
use crate::reclaim::incremental::{IncrementalVacuumError, incremental_vacuum};
use crate::reclaim::vacuum_into::{VacuumIntoOptions, vacuum_into_with_observer};

use super::signal::SignalController;

const INCREMENTAL_PAGES_PER_BATCH: u32 = 1_000;

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

/// Bounds one delete batch when only aggregate candidate bytes are available.
///
/// Every ID-sorted delete batch is a subset of the selected candidates, so its attributable bytes
/// cannot exceed `selected_bytes`, regardless of how skewed their sizes or ordering are. Adding one
/// page preserves the allowance for SQLite's page-granular WAL accounting.
fn one_batch_wal_allowance(selected_bytes: u64, selected_count: usize, page_size: u32) -> u64 {
    if selected_count == 0 {
        return u64::from(page_size);
    }
    selected_bytes.saturating_add(u64::from(page_size))
}

fn incremental_error(error: IncrementalVacuumError) -> Error {
    match error {
        IncrementalVacuumError::Sqlite { context, source } => Error::Sqlite {
            context: context.to_owned(),
            source,
        },
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
    use crate::delete::sessions::DEFAULT_BATCH_SIZE;

    #[test]
    fn wal_allowance_covers_all_selected_candidate_bytes() {
        let selected_count = DEFAULT_BATCH_SIZE.saturating_mul(2);
        let selected_bytes = u64::try_from(selected_count)
            .expect("test count should fit u64")
            .saturating_mul(10);
        assert_eq!(
            one_batch_wal_allowance(selected_bytes, selected_count, 4_096),
            selected_bytes.saturating_add(4_096)
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
}
