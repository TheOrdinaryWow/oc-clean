use crate::assets::snapshot::{self, GcSnapshotsOutcome, RemovalReport};
use crate::assets::storage::{self, SweepReport, SweepScope};
use crate::db::{DatabaseConnection, ReadWrite};
use crate::delete::projects::{self, ProjectIds};
use crate::paths::DerivedPaths;
use crate::select::predicates::SessionIds;

#[derive(Debug, Default)]
pub(super) struct CleanupOutcome {
    pub(super) storage_files_removed: u64,
    pub(super) snapshot_directories_removed: u64,
    pub(super) partial_failures: Vec<String>,
}

impl CleanupOutcome {
    pub(super) fn sweep_this_run(
        &mut self,
        database: &DatabaseConnection<ReadWrite>,
        paths: &DerivedPaths,
        deleted_session_ids: &SessionIds,
    ) {
        match storage::sweep(
            database,
            &paths.storage,
            SweepScope::ThisRun(deleted_session_ids),
        ) {
            Ok(report) => self.record_sweep(report),
            Err(error) => self.partial_failures.push(error.to_string()),
        }
    }

    pub(super) fn remove_pruned(&mut self, paths: &DerivedPaths, pruned: &ProjectIds) {
        match snapshot::remove_pruned(&paths.snapshot, pruned) {
            Ok(report) => self.record_removal(report),
            Err(error) => self.partial_failures.push(error.to_string()),
        }
    }

    pub(super) fn remove_orphaned(
        &mut self,
        database: &DatabaseConnection<ReadWrite>,
        paths: &DerivedPaths,
    ) {
        match snapshot::remove_orphaned(database, &paths.snapshot) {
            Ok(report) => self.record_removal(report),
            Err(error) => self.partial_failures.push(error.to_string()),
        }
    }

    pub(super) fn sweep_preexisting_orphans(
        &mut self,
        database: &DatabaseConnection<ReadWrite>,
        paths: &DerivedPaths,
    ) {
        self.remove_orphaned(database, paths);
        match storage::sweep(database, &paths.storage, SweepScope::AllOrphans) {
            Ok(report) => self.record_sweep(report),
            Err(error) => self.partial_failures.push(error.to_string()),
        }
    }

    pub(super) fn gc_retained(
        &mut self,
        database: &DatabaseConnection<ReadWrite>,
        paths: &DerivedPaths,
        pruned: &ProjectIds,
    ) {
        let retained = match projects::all_ids(database) {
            Ok(retained) => retained,
            Err(error) => {
                self.partial_failures.push(error.to_string());
                return;
            }
        };
        match snapshot::gc_retained(&paths.snapshot, &retained, pruned) {
            Ok(GcSnapshotsOutcome::Completed(report)) => {
                if let Some(error) = report.partial_success_error() {
                    self.partial_failures.push(error.to_string());
                }
            }
            Ok(GcSnapshotsOutcome::SkippedGitUnavailable { warning }) => {
                self.partial_failures.push(warning);
            }
            Err(error) => self.partial_failures.push(error.to_string()),
        }
    }

    fn record_sweep(&mut self, report: SweepReport) {
        self.storage_files_removed = self
            .storage_files_removed
            .saturating_add(report.deleted_files);
        self.partial_failures.extend(
            report
                .file_errors
                .into_iter()
                .map(|failure| failure.path.display().to_string()),
        );
    }

    fn record_removal(&mut self, report: RemovalReport) {
        self.snapshot_directories_removed = self
            .snapshot_directories_removed
            .saturating_add(report.removed_directories);
        self.partial_failures.extend(
            report
                .directory_errors
                .into_iter()
                .map(|failure| failure.path.display().to_string()),
        );
    }
}
