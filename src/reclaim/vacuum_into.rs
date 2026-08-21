use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OptionalExtension};
use tracing::warn;

use crate::db::{AnchoredDatabaseFile, FileIdentity, ReadWriteConnection};
use crate::error::Error;
use crate::report::progress;

use super::platform;

static G_BACKUP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How often the rebuild progress poller re-reads the output file's size.
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VacuumIntoOptions {
    pub skip_backup: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VacuumIntoReport {
    pub original_bytes: u64,
    pub compacted_bytes: u64,
    pub bytes_reclaimed: u64,
    pub backup_path: Option<PathBuf>,
}

/// Observes the atomic filesystem swap boundary without changing the swap algorithm.
pub trait SwapObserver {
    /// Enters the signal-deferred section before source sidecars are touched.
    ///
    /// # Errors
    ///
    /// Returns a typed interruption when cancellation was already pending.
    fn enter(&self) -> Result<(), Error>;

    /// Leaves the signal-deferred section after the rename attempt finishes.
    fn leave(&self);
}

/// Rebuilds a locked database into a verified sibling file and atomically replaces the source.
///
/// `locked_data_version` must have been read from `database` immediately after the caller acquired
/// its exclusive lock. The connection is consumed so this function can close it before entering
/// the filesystem swap critical section.
///
/// # Errors
///
/// Returns a typed database, integrity, or filesystem error while preserving the canonical source
/// path on every pre-swap and rename failure.
pub fn vacuum_into(
    database: ReadWriteConnection,
    database_path: &Path,
    locked_data_version: i64,
    options: VacuumIntoOptions,
) -> Result<VacuumIntoReport, Error> {
    vacuum_into_with_runtime(
        database,
        database_path,
        locked_data_version,
        options,
        &SystemFileOperations,
        &NoopHooks,
    )
}

/// Rebuilds and swaps a database while reporting the exact atomic swap boundary.
///
/// # Errors
///
/// Returns the same typed database, integrity, filesystem, and interruption errors as
/// [`vacuum_into`].
pub fn vacuum_into_with_observer(
    database: ReadWriteConnection,
    database_path: &Path,
    locked_data_version: i64,
    options: VacuumIntoOptions,
    observer: &dyn SwapObserver,
) -> Result<VacuumIntoReport, Error> {
    vacuum_into_with_runtime(
        database,
        database_path,
        locked_data_version,
        options,
        &SystemFileOperations,
        &ObserverHooks { observer },
    )
}

#[derive(Debug, Eq, PartialEq)]
struct SourceSnapshot {
    page_size: i64,
    auto_vacuum: i64,
    journal_mode: String,
    user_version: i64,
    application_id: i64,
    table_counts: Vec<(String, i64)>,
}

trait FileOperations {
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn copy(&self, source: &Path, destination: &Path) -> io::Result<u64>;
    fn copy_exclusive(&self, source: &Path, destination: &Path) -> io::Result<u64> {
        copy_exclusive(source, destination)
    }
    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn hard_link_anchored(
        &self,
        anchor: &AnchoredDatabaseFile,
        destination: &Path,
    ) -> io::Result<()> {
        self.hard_link(anchor.resolved_path(), destination)
    }
    fn copy_anchored_exclusive(
        &self,
        anchor: &AnchoredDatabaseFile,
        destination: &Path,
    ) -> io::Result<u64> {
        self.copy_exclusive(anchor.resolved_path(), destination)
    }
    fn rename_anchored(&self, anchor: &AnchoredDatabaseFile, source: &Path) -> io::Result<()> {
        self.rename(source, anchor.resolved_path())
    }
    fn remove_anchored(&self, _anchor: &AnchoredDatabaseFile, path: &Path) -> io::Result<()> {
        self.remove_file(path)
    }
}

struct SystemFileOperations;

impl FileOperations for SystemFileOperations {
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
        fs::hard_link(source, destination)
    }

    fn copy(&self, source: &Path, destination: &Path) -> io::Result<u64> {
        fs::copy(source, destination)
    }

    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
        platform::rename_over(source, destination)
    }

    #[cfg(unix)]
    fn hard_link_anchored(
        &self,
        anchor: &AnchoredDatabaseFile,
        destination: &Path,
    ) -> io::Result<()> {
        anchor.hard_link_to(destination)
    }

    #[cfg(unix)]
    fn copy_anchored_exclusive(
        &self,
        anchor: &AnchoredDatabaseFile,
        destination: &Path,
    ) -> io::Result<u64> {
        anchor.copy_to_exclusive(destination)
    }

    #[cfg(unix)]
    fn rename_anchored(&self, anchor: &AnchoredDatabaseFile, source: &Path) -> io::Result<()> {
        anchor.rename_sibling_over_source(source)
    }

    #[cfg(unix)]
    fn remove_anchored(&self, anchor: &AnchoredDatabaseFile, path: &Path) -> io::Result<()> {
        anchor.remove_sibling(path)
    }
}

trait SwapHooks {
    fn timestamp(&self) -> Option<String> {
        None
    }
    fn after_vacuum(&self, _source: &Path, _temporary: &Path) {}
    fn after_lock_closed(&self, _source: &Path, _temporary: &Path) {}
    fn enter_swap(&self) -> Result<(), Error> {
        Ok(())
    }
    fn leave_swap(&self) {}
    fn backup_created(&self, _backup: &Path) {}
    fn after_rename(&self, _database: &Path) {}
}

struct NoopHooks;

impl SwapHooks for NoopHooks {}

struct ObserverHooks<'observer> {
    observer: &'observer dyn SwapObserver,
}

impl SwapHooks for ObserverHooks<'_> {
    fn enter_swap(&self) -> Result<(), Error> {
        self.observer.enter()
    }

    fn leave_swap(&self) {
        self.observer.leave();
    }
}

struct TemporaryDatabase<'ops, Ops: FileOperations> {
    path: PathBuf,
    operations: &'ops Ops,
}

impl<Ops: FileOperations> Drop for TemporaryDatabase<'_, Ops> {
    fn drop(&mut self) {
        remove_if_present(self.operations, &self.path);
        remove_if_present(self.operations, &sidecar_path(&self.path, "-wal"));
        remove_if_present(self.operations, &sidecar_path(&self.path, "-shm"));
    }
}

fn vacuum_into_with_runtime<Ops: FileOperations, Hooks: SwapHooks>(
    database: ReadWriteConnection,
    database_path: &Path,
    locked_data_version: i64,
    options: VacuumIntoOptions,
    operations: &Ops,
    hooks: &Hooks,
) -> Result<VacuumIntoReport, Error> {
    validate_file_target(database_path)?;
    let requested_database_path = database_path;
    let database_anchor = database.database_anchor()?;
    database_anchor.ensure_path_matches(
        requested_database_path,
        "database target changed before VACUUM INTO",
    )?;
    let database_path = database_anchor.resolved_path();
    database_anchor
        .ensure_resolved_path_matches("resolved database target changed before VACUUM INTO")?;
    let anchored_database_path = database_anchor.inspection_path();
    let original_bytes = file_size(&anchored_database_path)?;
    checkpoint_source(database.connection())?;
    let snapshot = source_snapshot(database.connection())?;
    let timestamp = match hooks.timestamp() {
        Some(timestamp) => timestamp,
        None => utc_compact_timestamp(database.connection())?,
    };
    let temporary_display_path = generated_path(database_path, ".oc-clean-tmp-", &timestamp)?;
    let temporary_path = database_anchor.sibling_path(&temporary_display_path)?;
    ensure_available_path(&temporary_path)?;
    let temporary = TemporaryDatabase {
        path: temporary_path,
        operations,
    };

    vacuum_to(database.connection(), &temporary.path)?;
    hooks.after_vacuum(database_path, &temporary_display_path);
    verify_output(&temporary.path, &snapshot)?;
    restore_wal_and_verify_auto_vacuum(&temporary.path, snapshot.auto_vacuum)?;

    if database.data_version()? != locked_data_version {
        return Err(database_busy("source data_version changed before swap"));
    }
    let identity = database_anchor.identity()?;
    ensure_wal_truncated(&anchored_database_path)?;
    let hard_links = database.capabilities().hard_links;
    drop(database);
    hooks.after_lock_closed(database_path, &temporary_display_path);
    database_anchor.ensure_path_matches(
        requested_database_path,
        "database target changed after the VACUUM lock closed",
    )?;
    database_anchor.ensure_resolved_path_matches(
        "resolved database target changed after the VACUUM lock closed",
    )?;

    let proposed_backup_path = generated_path(database_path, ".bak.", &timestamp)?;
    hooks.enter_swap()?;
    let swap_result = swap_critical_section(
        requested_database_path,
        database_path,
        database_anchor.as_ref(),
        &temporary_display_path,
        &proposed_backup_path,
        identity,
        hard_links,
        options.skip_backup,
        operations,
        hooks,
    );
    hooks.leave_swap();
    let backup_path = swap_result?;

    hooks.after_rename(database_path);
    sync_file_and_parent(&anchored_database_path)?;
    if let Err(verification_error) = verify_swapped_database(&anchored_database_path) {
        let rollback_backup = backup_path.as_deref().unwrap_or(&proposed_backup_path);
        let rollback_backup = database_anchor.sibling_path(rollback_backup)?;
        rollback_after_verification_failure(
            operations,
            &anchored_database_path,
            &rollback_backup,
            backup_path.is_some(),
        )?;
        return Err(verification_error);
    }
    let compacted_bytes = file_size(&anchored_database_path)?;
    cleanup_temporary_sidecars(operations, &temporary.path);
    if options.skip_backup
        && let Some(backup_path) = &backup_path
    {
        operations
            .remove_anchored(database_anchor.as_ref(), backup_path)
            .map_err(|source| io_error(backup_path, source))?;
    }

    Ok(VacuumIntoReport {
        original_bytes,
        compacted_bytes,
        bytes_reclaimed: original_bytes.saturating_sub(compacted_bytes),
        backup_path: if options.skip_backup {
            None
        } else {
            backup_path
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn swap_critical_section<Ops: FileOperations, Hooks: SwapHooks>(
    requested_database_path: &Path,
    database_path: &Path,
    database_anchor: &AnchoredDatabaseFile,
    temporary_path: &Path,
    backup_path: &Path,
    expected_identity: FileIdentity,
    hard_links: bool,
    skip_backup: bool,
    operations: &Ops,
    hooks: &Hooks,
) -> Result<Option<PathBuf>, Error> {
    database_anchor.ensure_path_matches(
        requested_database_path,
        "database target changed before swap",
    )?;
    database_anchor.ensure_resolved_path_matches("resolved database target changed before swap")?;
    if database_anchor.identity()? != expected_identity {
        return Err(database_busy("source file identity changed before swap"));
    }
    let anchored_database_path = database_anchor.inspection_path();
    ensure_sidecars_quiescent(&anchored_database_path)?;
    remove_quiescent_sidecars(operations, &anchored_database_path)?;

    let backup_path = create_recovery_link(
        operations,
        database_anchor,
        backup_path,
        hard_links,
        skip_backup,
    )?;
    if let Some(backup_path) = &backup_path {
        hooks.backup_created(backup_path);
    }
    database_anchor.ensure_path_matches(
        requested_database_path,
        "database target changed after the recovery backup was created",
    )?;
    database_anchor.ensure_resolved_path_matches(
        "resolved database target changed after the recovery backup was created",
    )?;
    database_anchor.ensure_identity(
        expected_identity,
        "anchored database changed before replacement",
    )?;
    if let Err(source) = operations.rename_anchored(database_anchor, temporary_path) {
        let failed_backup_cleanup = backup_path.as_ref().and_then(|backup_path| {
            operations
                .remove_anchored(database_anchor, backup_path)
                .is_err()
                .then(|| backup_path.clone())
        });
        if let Some(backup_path) = failed_backup_cleanup {
            return Err(Error::SwapRollbackFailed {
                database_path: database_path.to_path_buf(),
                backup_path,
            });
        }
        return Err(annotate_swap_failure(
            platform::rename_error(database_path, source),
            "renaming the compacted database over the source",
        ));
    }
    Ok(backup_path)
}

/// Adds the failing swap step to an error whose message would otherwise name only a path.
fn annotate_swap_failure(error: Error, step: &str) -> Error {
    match error {
        Error::Io { path, source } => Error::Io {
            path,
            source: io::Error::new(source.kind(), format!("{step}: {source}")),
        },
        other => other,
    }
}

fn checkpoint_source(connection: &Connection) -> Result<(), Error> {
    let busy = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|source| sqlite_error("checkpointing source WAL", source))?;
    if busy == 0 {
        Ok(())
    } else {
        Err(database_busy("source WAL checkpoint reported busy"))
    }
}

fn source_snapshot(connection: &Connection) -> Result<SourceSnapshot, Error> {
    Ok(SourceSnapshot {
        page_size: pragma_i64(connection, "page_size")?,
        auto_vacuum: pragma_i64(connection, "auto_vacuum")?,
        journal_mode: pragma_string(connection, "journal_mode")?,
        user_version: pragma_i64(connection, "user_version")?,
        application_id: pragma_i64(connection, "application_id")?,
        table_counts: table_counts(connection)?,
    })
}

fn verify_output(path: &Path, source: &SourceSnapshot) -> Result<(), Error> {
    let connection = open_output(path)?;
    verify_integrity(&connection)?;
    verify_foreign_keys(&connection)?;
    let output_journal_mode = pragma_string(&connection, "journal_mode")?;
    if !output_journal_mode.eq_ignore_ascii_case("delete") {
        return Err(integrity_error(
            "journal_mode",
            format!(
                "VACUUM output expected delete before restoring source mode {}, found {output_journal_mode}",
                source.journal_mode
            ),
        ));
    }
    compare_value(
        "page_size",
        pragma_i64(&connection, "page_size")?,
        source.page_size,
    )?;
    compare_value(
        "auto_vacuum",
        pragma_i64(&connection, "auto_vacuum")?,
        source.auto_vacuum,
    )?;
    compare_value(
        "user_version",
        pragma_i64(&connection, "user_version")?,
        source.user_version,
    )?;
    compare_value(
        "application_id",
        pragma_i64(&connection, "application_id")?,
        source.application_id,
    )?;
    let counts = table_counts(&connection)?;
    if counts != source.table_counts {
        return Err(integrity_error(
            "table row counts",
            format!("source {:?}, output {counts:?}", source.table_counts),
        ));
    }
    Ok(())
}

fn restore_wal_and_verify_auto_vacuum(path: &Path, expected_auto_vacuum: i64) -> Result<(), Error> {
    let connection = open_output(path)?;
    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|source| sqlite_error("restoring output journal_mode=WAL", source))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(integrity_error(
            "journal_mode",
            format!("expected wal, found {journal_mode}"),
        ));
    }
    compare_value(
        "auto_vacuum",
        pragma_i64(&connection, "auto_vacuum")?,
        expected_auto_vacuum,
    )
}

fn verify_swapped_database(path: &Path) -> Result<(), Error> {
    let connection = open_output(path)?;
    let journal_mode = pragma_string(&connection, "journal_mode")?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(integrity_error(
            "journal_mode",
            format!("expected wal after swap, found {journal_mode}"),
        ));
    }
    verify_integrity(&connection)
}

fn verify_integrity(connection: &Connection) -> Result<(), Error> {
    let result = pragma_string(connection, "integrity_check")?;
    if result == "ok" {
        Ok(())
    } else {
        Err(integrity_error("integrity_check", result))
    }
}

fn verify_foreign_keys(connection: &Connection) -> Result<(), Error> {
    let finding = connection
        .query_row("PRAGMA foreign_key_check", [], |row| {
            let table: String = row.get(0)?;
            let row_id: Option<i64> = row.get(1)?;
            Ok(format!("table {table}, row {row_id:?}"))
        })
        .optional()
        .map_err(|source| sqlite_error("running output foreign_key_check", source))?;
    match finding {
        Some(message) => Err(integrity_error("foreign_key_check", message)),
        None => Ok(()),
    }
}

fn table_counts(connection: &Connection) -> Result<Vec<(String, i64)>, Error> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .map_err(|source| sqlite_error("preparing table enumeration", source))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| sqlite_error("enumerating database tables", source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error("reading database table name", source))?;
    names
        .into_iter()
        .map(|name| {
            let sql = format!("SELECT count(*) FROM {}", quote_identifier(&name));
            connection
                .query_row(&sql, [], |row| row.get::<_, i64>(0))
                .map(|count| (name, count))
                .map_err(|source| sqlite_error("counting table rows", source))
        })
        .collect()
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn vacuum_to(connection: &Connection, path: &Path) -> Result<(), Error> {
    let path_value = path.to_str().ok_or_else(|| Error::InvalidArgument {
        argument: path.display().to_string(),
        reason: "SQLite VACUUM INTO requires a UTF-8 path".to_owned(),
    })?;
    let projected_bytes = projected_output_bytes(connection);
    let bar = progress::bytes("rebuild", projected_bytes);
    bar.set_message("rebuilding the database");
    let done = AtomicBool::new(false);

    // SQLite exposes no progress callback for VACUUM INTO, so the rebuilt file's own growth
    // toward the projected live size is the only available signal. It is an approximation:
    // the projection ignores per-page overhead differences between source and rebuild.
    let result = thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                thread::sleep(OUTPUT_POLL_INTERVAL);
                if let Ok(metadata) = fs::metadata(path) {
                    bar.set_position(metadata.len().min(projected_bytes));
                }
            }
        });
        let result = connection.execute("VACUUM INTO ?1", [path_value]);
        done.store(true, Ordering::Relaxed);
        result
    });

    bar.finish();
    result
        .map(|_| ())
        .map_err(|source| match source.sqlite_error_code() {
            Some(ErrorCode::CannotOpen | ErrorCode::ReadOnly) => io_error(
                path,
                io::Error::new(io::ErrorKind::PermissionDenied, source.to_string()),
            ),
            _ => sqlite_error("running VACUUM INTO", source),
        })
}

/// Estimates the rebuilt file's final size from the source's live pages.
///
/// Returns zero when the pragmas cannot be read, which renders an indeterminate bar rather
/// than a wrong percentage.
fn projected_output_bytes(connection: &Connection) -> u64 {
    let pragma = |name: &str| -> Option<u64> {
        connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
            .ok()
            .and_then(|value| u64::try_from(value).ok())
    };
    let page_count = pragma("page_count").unwrap_or(0);
    let freelist_count = pragma("freelist_count").unwrap_or(0);
    let page_size = pragma("page_size").unwrap_or(0);
    page_count
        .saturating_sub(freelist_count)
        .saturating_mul(page_size)
}

fn utc_compact_timestamp(connection: &Connection) -> Result<String, Error> {
    connection
        .query_row("SELECT strftime('%Y%m%dT%H%M%SZ', 'now')", [], |row| {
            row.get(0)
        })
        .map_err(|source| sqlite_error("generating compact UTC timestamp", source))
}

fn generated_path(
    database_path: &Path,
    separator: &str,
    timestamp: &str,
) -> Result<PathBuf, Error> {
    let filename = database_path
        .file_name()
        .ok_or_else(|| Error::InvalidArgument {
            argument: database_path.display().to_string(),
            reason: "database path has no filename".to_owned(),
        })?;
    let mut generated = OsString::from(filename);
    generated.push(separator);
    generated.push(timestamp);
    Ok(database_path.with_file_name(generated))
}

fn sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(database_path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

fn ensure_sidecars_quiescent(database_path: &Path) -> Result<(), Error> {
    for path in [
        sidecar_path(database_path, "-wal"),
        sidecar_path(database_path, "-shm"),
    ] {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.len() > 0 => {
                return Err(database_busy("source sidecar is not empty before swap"));
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&path, source)),
        }
    }
    Ok(())
}

fn ensure_wal_truncated(database_path: &Path) -> Result<(), Error> {
    let path = sidecar_path(database_path, "-wal");
    match fs::metadata(&path) {
        Ok(metadata) if metadata.len() > 0 => {
            Err(database_busy("source WAL is not empty before closing lock"))
        }
        Ok(_) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(&path, source)),
    }
}

fn remove_quiescent_sidecars<Ops: FileOperations>(
    operations: &Ops,
    database_path: &Path,
) -> Result<(), Error> {
    for path in [
        sidecar_path(database_path, "-wal"),
        sidecar_path(database_path, "-shm"),
    ] {
        match operations.remove_file(&path) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&path, source)),
        }
    }
    Ok(())
}

fn create_recovery_link<Ops: FileOperations>(
    operations: &Ops,
    database_anchor: &AnchoredDatabaseFile,
    backup_path: &Path,
    hard_links: bool,
    skip_backup: bool,
) -> Result<Option<PathBuf>, Error> {
    if skip_backup && !hard_links {
        return Ok(None);
    }

    refuse_destination_symlink(&database_anchor.sibling_path(backup_path)?)?;
    loop {
        let candidate = available_backup_candidate(database_anchor, backup_path)?;
        refuse_destination_symlink(&database_anchor.sibling_path(&candidate)?)?;
        let result = if hard_links {
            operations
                .hard_link_anchored(database_anchor, &candidate)
                .map(|()| 0)
        } else {
            operations.copy_anchored_exclusive(database_anchor, &candidate)
        };
        match result {
            Ok(_) => return Ok(Some(candidate)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                refuse_destination_symlink(&database_anchor.sibling_path(&candidate)?)?;
            }
            Err(source) => return Err(io_error(&candidate, source)),
        }
    }
}

fn available_backup_candidate(
    database_anchor: &AnchoredDatabaseFile,
    backup_path: &Path,
) -> Result<PathBuf, Error> {
    let anchored_path = database_anchor.sibling_path(backup_path)?;
    match fs::symlink_metadata(&anchored_path) {
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(backup_path.to_path_buf()),
        Err(source) => Err(io_error(backup_path, source)),
        Ok(_) => {
            let filename = backup_path
                .file_name()
                .ok_or_else(|| Error::InvalidArgument {
                    argument: backup_path.display().to_string(),
                    reason: "backup path has no filename".to_owned(),
                })?;
            let sequence = G_BACKUP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut candidate = OsString::from(filename);
            candidate.push(format!("-{}-{sequence}", std::process::id()));
            Ok(backup_path.with_file_name(candidate))
        }
    }
}

fn refuse_destination_symlink(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io_error(
            path,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "backup destination is a symbolic link",
            ),
        )),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn copy_exclusive(source: &Path, destination: &Path) -> io::Result<u64> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "backup destination already exists",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut source_file = fs::File::open(source)?;
    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = io::copy(&mut source_file, &mut destination_file).and_then(|bytes| {
        destination_file.set_permissions(source_file.metadata()?.permissions())?;
        Ok(bytes)
    });
    match result {
        Ok(bytes) => Ok(bytes),
        Err(error) => {
            drop(destination_file);
            let _ = fs::remove_file(destination);
            Err(error)
        }
    }
}

fn cleanup_temporary_sidecars<Ops: FileOperations>(operations: &Ops, temporary_path: &Path) {
    for path in [
        sidecar_path(temporary_path, "-wal"),
        sidecar_path(temporary_path, "-shm"),
    ] {
        if let Err(source) = operations.remove_file(&path)
            && source.kind() != io::ErrorKind::NotFound
        {
            warn!(path = %path.display(), error = %source, "failed to remove temporary database sidecar");
        }
    }
}

fn rollback_after_verification_failure<Ops: FileOperations>(
    operations: &Ops,
    database_path: &Path,
    backup_path: &Path,
    backup_created: bool,
) -> Result<(), Error> {
    let sidecars_removed = [
        sidecar_path(database_path, "-wal"),
        sidecar_path(database_path, "-shm"),
    ]
    .into_iter()
    .all(|path| match operations.remove_file(&path) {
        Ok(()) => true,
        Err(source) => source.kind() == io::ErrorKind::NotFound,
    });
    if !backup_created
        || !sidecars_removed
        || operations.copy(backup_path, database_path).is_err()
        || operations.remove_file(backup_path).is_err()
        || sync_file_and_parent(database_path).is_err()
    {
        return Err(Error::SwapRollbackFailed {
            database_path: database_path.to_path_buf(),
            backup_path: backup_path.to_path_buf(),
        });
    }
    Ok(())
}

fn sync_file_and_parent(path: &Path) -> Result<(), Error> {
    // Flushing needs a writable handle: Windows denies FlushFileBuffers on a read-only handle,
    // while unix accepts fsync on either.
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error(path, source))?;
    sync_parent_directory(path)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), Error> {
    let parent = path.parent().ok_or_else(|| Error::InvalidArgument {
        argument: path.display().to_string(),
        reason: "database path has no parent directory".to_owned(),
    })?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(parent, source))
}

#[cfg(windows)]
fn sync_parent_directory(path: &Path) -> Result<(), Error> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let parent = path.parent().ok_or_else(|| Error::InvalidArgument {
        argument: path.display().to_string(),
        reason: "database path has no parent directory".to_owned(),
    })?;
    // Opening the directory confirms it is still reachable. Flushing it is neither possible nor
    // needed: Windows refuses FlushFileBuffers on a directory handle, and the swap already used
    // MOVEFILE_WRITE_THROUGH, which commits the directory entry before returning.
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)
        .map(drop)
        .map_err(|source| io_error(parent, source))
}

#[cfg(not(any(unix, windows)))]
fn sync_parent_directory(_path: &Path) -> Result<(), Error> {
    Err(Error::UnsupportedPlatform {
        platform: std::env::consts::OS.to_owned(),
    })
}

fn remove_if_present<Ops: FileOperations>(operations: &Ops, path: &Path) {
    if let Err(source) = operations.remove_file(path)
        && source.kind() != io::ErrorKind::NotFound
    {
        warn!(path = %path.display(), error = %source, "failed to clean temporary database file");
    }
}

fn open_output(path: &Path) -> Result<Connection, Error> {
    Connection::open(path).map_err(|source| sqlite_error("opening VACUUM output", source))
}

fn pragma_i64(connection: &Connection, pragma: &str) -> Result<i64, Error> {
    connection
        .pragma_query_value(None, pragma, |row| row.get(0))
        .map_err(|source| sqlite_error(&format!("reading PRAGMA {pragma}"), source))
}

fn pragma_string(connection: &Connection, pragma: &str) -> Result<String, Error> {
    connection
        .pragma_query_value(None, pragma, |row| row.get(0))
        .map_err(|source| sqlite_error(&format!("reading PRAGMA {pragma}"), source))
}

fn compare_value(name: &str, actual: i64, expected: i64) -> Result<(), Error> {
    if actual == expected {
        Ok(())
    } else {
        Err(integrity_error(
            name,
            format!("expected {expected}, found {actual}"),
        ))
    }
}

fn validate_file_target(path: &Path) -> Result<(), Error> {
    if path == Path::new(OsStr::new(":memory:")) {
        Err(Error::InvalidArgument {
            argument: ":memory:".to_owned(),
            reason: "VACUUM INTO requires a resolved file-backed database".to_owned(),
        })
    } else {
        Ok(())
    }
}

fn ensure_available_path(path: &Path) -> Result<(), Error> {
    if path.exists() {
        Err(io_error(
            path,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "generated path already exists",
            ),
        ))
    } else {
        Ok(())
    }
}

fn file_size(path: &Path) -> Result<u64, Error> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|source| io_error(path, source))
}

fn database_busy(reason: &str) -> Error {
    Error::DatabaseBusy {
        holders: vec![reason.to_owned()],
    }
}

fn integrity_error(check: &str, message: String) -> Error {
    Error::IntegrityCheckFailed {
        check: check.to_owned(),
        message,
    }
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    if matches!(
        source.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    ) {
        database_busy(context)
    } else {
        Error::Sqlite {
            context: context.to_owned(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::fs;
    use std::hash::{Hash, Hasher};
    use std::io::{Read, Seek, SeekFrom, Write};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use rusqlite::{Connection, params};
    use tempfile::TempDir;

    use super::*;
    #[cfg(unix)]
    use crate::db::anchor_for_holder_scan;
    use crate::db::{ConnectionOptions, open_read_write};
    use crate::paths::Target;

    const USER_VERSION: i64 = 42;
    const APPLICATION_ID: i64 = 0x0C_C1_EA;

    struct Fixture {
        _directory: TempDir,
        path: PathBuf,
        original_bytes: Vec<u8>,
        original_len: u64,
    }

    impl Fixture {
        fn new(filename: &str) -> Self {
            let directory = tempfile::tempdir().expect("temporary directory should be created");
            let path = directory.path().join(filename);
            let connection = Connection::open(&path).expect("fixture database should open");
            connection
                .execute_batch(
                    "PRAGMA page_size = 4096;
                     PRAGMA auto_vacuum = INCREMENTAL;
                     PRAGMA journal_mode = WAL;
                     PRAGMA user_version = 42;
                     PRAGMA application_id = 836074;
                     CREATE TABLE alpha (id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
                     CREATE TABLE beta (id INTEGER PRIMARY KEY, alpha_id INTEGER NOT NULL, label TEXT NOT NULL);",
                )
                .expect("fixture schema should be created");
            let payload = vec![0x5a_u8; 16 * 1024];
            for id in 0..96_i64 {
                connection
                    .execute(
                        "INSERT INTO alpha (id, payload) VALUES (?1, ?2)",
                        params![id, payload],
                    )
                    .expect("fixture payload should be inserted");
                connection
                    .execute(
                        "INSERT INTO beta (id, alpha_id, label) VALUES (?1, ?1, printf('row-%d', ?1))",
                        [id],
                    )
                    .expect("fixture label should be inserted");
            }
            connection
                .execute("DELETE FROM alpha WHERE id % 2 = 0", [])
                .expect("fixture bloat should be created");
            connection
                .execute("DELETE FROM beta WHERE id % 2 = 0", [])
                .expect("fixture bloat should be created");
            connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .expect("fixture WAL should checkpoint");
            drop(connection);
            let original_bytes = fs::read(&path).expect("fixture bytes should be readable");
            let original_len =
                u64::try_from(original_bytes.len()).expect("fixture size should fit");
            Self {
                _directory: directory,
                path,
                original_bytes,
                original_len,
            }
        }

        fn locked(&self) -> (ReadWriteConnection, i64) {
            let database = open_read_write(
                &Target::File(self.path.clone()),
                ConnectionOptions::default(),
            )
            .expect("fixture should open read-write");
            database
                .acquire_exclusive_lock()
                .expect("fixture should acquire an exclusive lock");
            let data_version = database
                .data_version()
                .expect("fixture data version should be readable");
            (database, data_version)
        }
    }

    fn run(fixture: &Fixture, skip_backup: bool) -> VacuumIntoReport {
        let (database, data_version) = fixture.locked();
        vacuum_into(
            database,
            &fixture.path,
            data_version,
            VacuumIntoOptions { skip_backup },
        )
        .expect("VACUUM INTO should succeed")
    }

    fn pragma_i64(connection: &Connection, pragma: &str) -> i64 {
        connection
            .pragma_query_value(None, pragma, |row| row.get(0))
            .expect("pragma should be readable")
    }

    fn assert_verified_database(path: &Path) {
        let connection = Connection::open(path).expect("swapped database should open");
        let integrity: String = connection
            .pragma_query_value(None, "integrity_check", |row| row.get(0))
            .expect("integrity check should run");
        let journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("journal mode should be readable");
        assert_eq!(integrity, "ok");
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(pragma_i64(&connection, "user_version"), USER_VERSION);
        assert_eq!(pragma_i64(&connection, "application_id"), APPLICATION_ID);
        assert_eq!(pragma_i64(&connection, "auto_vacuum"), 2);
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM alpha", [], |row| row.get::<_, i64>(0))
                .expect("alpha count should be readable"),
            48
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM beta", [], |row| row.get::<_, i64>(0))
                .expect("beta count should be readable"),
            48
        );
    }

    fn content_hash(path: &Path) -> u64 {
        let bytes = fs::read(path).expect("file should be hashable");
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        hasher.finish()
    }

    fn directory_names(path: &Path) -> Vec<String> {
        let mut names = fs::read_dir(path)
            .expect("directory should be readable")
            .map(|entry| {
                entry
                    .expect("entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn run_with<Ops: FileOperations, Hooks: SwapHooks>(
        fixture: &Fixture,
        options: VacuumIntoOptions,
        operations: &Ops,
        hooks: &Hooks,
    ) -> Result<VacuumIntoReport, Error> {
        let (database, data_version) = fixture.locked();
        vacuum_into_with_runtime(
            database,
            &fixture.path,
            data_version,
            options,
            operations,
            hooks,
        )
    }

    #[derive(Default)]
    struct RecordingHooks {
        temporary_path: Mutex<Option<PathBuf>>,
        backup_path: Mutex<Option<PathBuf>>,
    }

    impl SwapHooks for RecordingHooks {
        fn after_vacuum(&self, _source: &Path, temporary: &Path) {
            *self.temporary_path.lock().expect("temporary path lock") =
                Some(temporary.to_path_buf());
        }

        fn backup_created(&self, backup: &Path) {
            *self.backup_path.lock().expect("backup path lock") = Some(backup.to_path_buf());
        }
    }

    struct CorruptOutputHooks {
        temporary_path: Mutex<Option<PathBuf>>,
    }

    impl SwapHooks for CorruptOutputHooks {
        fn after_vacuum(&self, _source: &Path, temporary: &Path) {
            *self.temporary_path.lock().expect("temporary path lock") =
                Some(temporary.to_path_buf());
            corrupt_btree_page(temporary);
        }
    }

    fn corrupt_btree_page(path: &Path) {
        let connection = Connection::open(path).expect("output should open before corruption");
        let page_size = pragma_i64(&connection, "page_size");
        let page_number: i64 = connection
            .query_row(
                "SELECT pageno FROM dbstat
                 WHERE name = 'alpha' AND pagetype IN ('internal', 'leaf') AND pageno > 1
                 ORDER BY pageno LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("a non-root alpha b-tree page should exist");
        drop(connection);
        let offset =
            u64::try_from((page_number - 1) * page_size).expect("fixture page offset should fit");
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("output should open for corruption");
        file.seek(SeekFrom::Start(offset))
            .expect("corruption offset should seek");
        let mut original = [0_u8; 1];
        file.read_exact(&mut original)
            .expect("page type should be readable");
        file.seek(SeekFrom::Start(offset))
            .expect("corruption offset should seek again");
        file.write_all(&[0xff])
            .expect("page type should be corrupted");
        file.sync_all().expect("corruption should reach disk");
    }

    struct ConcurrentWriterHooks {
        source: PathBuf,
        writer: Mutex<Option<thread::JoinHandle<()>>>,
    }

    impl ConcurrentWriterHooks {
        fn new(source: PathBuf) -> Self {
            Self {
                source,
                writer: Mutex::new(None),
            }
        }
    }

    impl SwapHooks for ConcurrentWriterHooks {
        fn after_vacuum(&self, _source: &Path, _temporary: &Path) {
            let path = self.source.clone();
            let (ready_tx, ready_rx) = mpsc::sync_channel(0);
            let writer = thread::spawn(move || {
                let connection = Connection::open(path).expect("writer should open source");
                connection
                    .busy_timeout(Duration::from_secs(10))
                    .expect("writer timeout should configure");
                ready_tx.send(()).expect("writer should signal readiness");
                connection
                    .execute(
                        "INSERT INTO alpha (id, payload) VALUES (999, zeroblob(32))",
                        [],
                    )
                    .expect("writer should commit after lock closes");
            });
            ready_rx.recv().expect("writer should become ready");
            *self.writer.lock().expect("writer lock") = Some(writer);
        }

        fn after_lock_closed(&self, _source: &Path, _temporary: &Path) {
            self.writer
                .lock()
                .expect("writer lock")
                .take()
                .expect("writer should exist")
                .join()
                .expect("writer should finish");
        }
    }

    struct WriterAfterCloseHooks;

    impl SwapHooks for WriterAfterCloseHooks {
        fn after_lock_closed(&self, source: &Path, _temporary: &Path) {
            let connection = Connection::open(source).expect("writer should open source");
            connection
                .execute(
                    "INSERT INTO alpha (id, payload) VALUES (1000, zeroblob(32))",
                    [],
                )
                .expect("writer should commit in close-to-swap window");
        }
    }

    /// Compares two paths by file identity so a symlinked prefix does not change the answer.
    fn same_file_path(left: &Path, right: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) else {
                return left == right;
            };
            left.dev() == right.dev() && left.ino() == right.ino()
        }
        #[cfg(not(unix))]
        {
            left.as_os_str().to_string_lossy().to_lowercase()
                == right.as_os_str().to_string_lossy().to_lowercase()
        }
    }

    struct FailingRenameOperations {
        hard_link_called: AtomicBool,
        canonical_existed_at_rename: AtomicBool,
        rename_source_was_canonical: AtomicBool,
    }

    struct InjectedSwapFailures {
        fail_rename: bool,
        fail_cleanup: bool,
        backup_path: Mutex<Option<PathBuf>>,
    }

    struct FailingSidecarRemovalOperations;

    impl FileOperations for FailingSidecarRemovalOperations {
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            if path
                .extension()
                .is_some_and(|extension| extension == "db-wal")
                || path.to_string_lossy().ends_with(".db-wal")
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected sidecar removal failure",
                ));
            }
            fs::remove_file(path)
        }

        fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
            fs::hard_link(source, destination)
        }

        fn copy(&self, source: &Path, destination: &Path) -> io::Result<u64> {
            fs::copy(source, destination)
        }

        fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
            fs::rename(source, destination)
        }
    }

    struct EmptySidecarHooks;

    impl SwapHooks for EmptySidecarHooks {
        fn after_lock_closed(&self, source: &Path, _temporary: &Path) {
            fs::write(sidecar_path(source, "-wal"), []).expect("empty WAL should be created");
        }
    }

    impl FailingRenameOperations {
        fn new() -> Self {
            Self {
                hard_link_called: AtomicBool::new(false),
                canonical_existed_at_rename: AtomicBool::new(false),
                rename_source_was_canonical: AtomicBool::new(false),
            }
        }
    }

    impl FileOperations for FailingRenameOperations {
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            fs::remove_file(path)
        }

        fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
            self.hard_link_called.store(true, Ordering::SeqCst);
            fs::hard_link(source, destination)
        }

        fn copy(&self, source: &Path, destination: &Path) -> io::Result<u64> {
            fs::copy(source, destination)
        }

        fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
            self.canonical_existed_at_rename
                .store(destination.exists(), Ordering::SeqCst);
            self.rename_source_was_canonical
                .store(same_file_path(source, destination), Ordering::SeqCst);
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected rename failure",
            ))
        }
    }

    impl InjectedSwapFailures {
        fn new(fail_rename: bool, fail_cleanup: bool) -> Self {
            Self {
                fail_rename,
                fail_cleanup,
                backup_path: Mutex::new(None),
            }
        }
    }

    impl FileOperations for InjectedSwapFailures {
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            let is_backup = self
                .backup_path
                .lock()
                .expect("backup path lock")
                .as_deref()
                == Some(path);
            if self.fail_cleanup && is_backup {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected backup cleanup failure",
                ));
            }
            fs::remove_file(path)
        }

        fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
            fs::hard_link(source, destination)?;
            *self.backup_path.lock().expect("backup path lock") = Some(destination.to_path_buf());
            Ok(())
        }

        fn copy(&self, source: &Path, destination: &Path) -> io::Result<u64> {
            fs::copy(source, destination)
        }

        fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
            if self.fail_rename {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected rename failure",
                ))
            } else {
                fs::rename(source, destination)
            }
        }
    }

    struct FixedTimestampHooks(&'static str);

    impl SwapHooks for FixedTimestampHooks {
        fn timestamp(&self) -> Option<String> {
            Some(self.0.to_owned())
        }
    }

    struct CorruptAfterRenameHooks;

    impl SwapHooks for CorruptAfterRenameHooks {
        fn after_rename(&self, database: &Path) {
            corrupt_btree_page(database);
        }
    }

    #[cfg(unix)]
    struct ReplaceTargetAfterBackupHooks {
        source: PathBuf,
        displaced: PathBuf,
    }

    #[cfg(unix)]
    impl SwapHooks for ReplaceTargetAfterBackupHooks {
        fn backup_created(&self, _backup: &Path) {
            fs::rename(&self.source, &self.displaced).expect("anchored source should be displaced");
            fs::write(&self.source, b"attacker replacement")
                .expect("attacker replacement should be created");
        }
    }

    #[test]
    fn verified_swap_shrinks_database_and_preserves_every_database_invariant() {
        let fixture = Fixture::new("opencode-nightly.db");
        let report = run(&fixture, false);

        assert_verified_database(&fixture.path);
        assert!(report.compacted_bytes < fixture.original_len);
        assert_eq!(report.original_bytes, fixture.original_len);
        assert_eq!(
            report.bytes_reclaimed,
            fixture.original_len - report.compacted_bytes
        );
        let backup = report.backup_path.expect("backup should be retained");
        assert_eq!(
            fs::read(backup).expect("backup should be readable"),
            fixture.original_bytes
        );
    }

    #[test]
    fn skip_backup_removes_backup_only_after_verified_success() {
        let fixture = Fixture::new("custom.db");
        let report = run(&fixture, true);

        assert_verified_database(&fixture.path);
        assert_eq!(report.backup_path, None);
        let names = fs::read_dir(fixture.path.parent().expect("fixture has a parent"))
            .expect("fixture directory should be readable")
            .map(|entry| entry.expect("entry should be readable").file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [fixture.path.file_name().expect("fixture has a filename")]
        );
    }

    /// Compares two directories by identity so a symlinked prefix does not fail the comparison.
    ///
    /// macOS spells the same temporary directory as both `/var/...` and `/private/var/...`.
    fn same_directory(left: &Path, right: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) else {
                return false;
            };
            left.dev() == right.dev() && left.ino() == right.ino()
        }
        #[cfg(not(unix))]
        {
            left.as_os_str().to_string_lossy().to_lowercase()
                == right.as_os_str().to_string_lossy().to_lowercase()
        }
    }

    #[test]
    fn generated_siblings_use_resolved_filename_and_windows_legal_timestamp() {
        let fixture = Fixture::new("opencode-nightly.db");
        let hooks = RecordingHooks::default();
        let report = run_with(
            &fixture,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &hooks,
        )
        .expect("custom filename swap should succeed");
        let backup = report.backup_path.expect("backup should be retained");
        let backup_name = backup
            .file_name()
            .expect("backup should have a filename")
            .to_string_lossy();
        let suffix = backup_name
            .strip_prefix("opencode-nightly.db.bak.")
            .expect("backup should derive from resolved filename");
        assert_eq!(suffix.len(), 16);
        assert_eq!(&suffix[8..9], "T");
        assert_eq!(&suffix[15..16], "Z");
        assert!(suffix[..8].bytes().all(|byte| byte.is_ascii_digit()));
        assert!(suffix[9..15].bytes().all(|byte| byte.is_ascii_digit()));
        assert!(!backup_name.bytes().any(|byte| b":*?\"<>|".contains(&byte)));

        let temporary = hooks
            .temporary_path
            .lock()
            .expect("temporary path lock")
            .clone()
            .expect("temporary path should be recorded");
        let temporary_parent = temporary
            .parent()
            .expect("temporary path should have a parent");
        let fixture_parent = fixture
            .path
            .parent()
            .expect("fixture path should have a parent");
        assert!(
            same_directory(temporary_parent, fixture_parent),
            "temporary sibling should live beside the database: {} vs {}",
            temporary_parent.display(),
            fixture_parent.display()
        );
        assert!(
            temporary
                .file_name()
                .expect("temporary path should have a filename")
                .to_string_lossy()
                .starts_with("opencode-nightly.db.oc-clean-tmp-")
        );
        assert_eq!(
            directory_names(fixture.path.parent().expect("fixture parent")),
            vec!["opencode-nightly.db".to_owned(), backup_name.into_owned()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_database_swap_replaces_target_and_preserves_lexical_link() {
        let fixture = Fixture::new("real.db");
        let link = fixture
            .path
            .parent()
            .expect("fixture should have a parent")
            .join("db.sqlite");
        symlink("real.db", &link).expect("database symlink should be created");
        let database = open_read_write(&Target::File(link.clone()), ConnectionOptions::default())
            .expect("symlinked fixture should open read-write");
        database
            .acquire_exclusive_lock()
            .expect("symlinked fixture should acquire an exclusive lock");
        let data_version = database
            .data_version()
            .expect("fixture data version should be readable");

        let report = vacuum_into_with_runtime(
            database,
            &link,
            data_version,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &NoopHooks,
        )
        .expect("symlinked database swap should succeed");

        assert!(
            fs::symlink_metadata(&link)
                .expect("database link metadata should be readable")
                .file_type()
                .is_symlink()
        );
        assert_verified_database(&fixture.path);
        assert!(fs::metadata(&fixture.path).expect("target metadata").len() < fixture.original_len);
        assert!(
            report
                .backup_path
                .expect("backup should be retained")
                .file_name()
                .expect("backup should have a filename")
                .to_string_lossy()
                .starts_with("real.db.bak.")
        );
    }

    #[test]
    fn repeated_backup_timestamp_retains_distinct_backups() {
        let fixture = Fixture::new("backup-collision.db");
        let timestamp = "20260821T123456Z";

        let first = run_with(
            &fixture,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &FixedTimestampHooks(timestamp),
        )
        .expect("first backup should succeed")
        .backup_path
        .expect("first backup should be retained");
        let second = run_with(
            &fixture,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &FixedTimestampHooks(timestamp),
        )
        .expect("second backup should choose a fresh path")
        .backup_path
        .expect("second backup should be retained");

        assert_ne!(first, second);
        assert!(first.is_file());
        assert!(second.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn copy_backup_refuses_existing_destination_symlink() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let source = directory.path().join("source.db");
        let destination = directory.path().join("backup.db");
        let redirected = directory.path().join("redirected.db");
        fs::write(&source, b"backup bytes").expect("source should be written");
        fs::write(&redirected, b"sentinel bytes").expect("sentinel should be written");
        symlink(&redirected, &destination).expect("destination symlink should be created");

        let anchor = anchor_for_holder_scan(&source).expect("source should be anchored");
        let error = create_recovery_link(
            &SystemFileOperations,
            anchor.as_ref(),
            &destination,
            false,
            false,
        )
        .expect_err("copy backup should refuse an existing destination symlink");

        assert!(matches!(error, Error::Io { .. }));
        assert_eq!(
            fs::read(&redirected).expect("sentinel should remain readable"),
            b"sentinel bytes"
        );
    }

    #[test]
    fn failed_output_integrity_preserves_source_and_removes_temporary_file() {
        let fixture = Fixture::new("integrity.db");
        let hooks = CorruptOutputHooks {
            temporary_path: Mutex::new(None),
        };
        let source_hash = content_hash(&fixture.path);
        let error = run_with(
            &fixture,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &hooks,
        )
        .expect_err("corrupt output should fail verification");

        assert!(matches!(
            error,
            Error::IntegrityCheckFailed { .. } | Error::Sqlite { .. }
        ));
        assert_eq!(content_hash(&fixture.path), source_hash);
        let temporary = hooks
            .temporary_path
            .lock()
            .expect("temporary path lock")
            .clone()
            .expect("temporary path should be recorded");
        assert!(!temporary.exists());
    }

    #[test]
    fn writer_started_after_copy_causes_busy_abort_and_retains_commit() {
        let fixture = Fixture::new("copy-window.db");
        let hooks = ConcurrentWriterHooks::new(fixture.path.clone());
        let error = run_with(
            &fixture,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &hooks,
        )
        .expect_err("concurrent write should abort swap");

        assert!(matches!(error, Error::DatabaseBusy { .. }));
        let connection = Connection::open(&fixture.path).expect("source should remain readable");
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM alpha WHERE id = 999", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("concurrent row count should be readable"),
            1
        );
        assert!(
            !directory_names(fixture.path.parent().expect("fixture parent"))
                .iter()
                .any(|name| name.contains("oc-clean-tmp"))
        );
    }

    #[test]
    fn writer_after_lock_close_is_detected_before_sidecar_deletion() {
        let fixture = Fixture::new("identity-window.db");
        let error = run_with(
            &fixture,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &WriterAfterCloseHooks,
        )
        .expect_err("close-window write should abort swap");

        assert!(matches!(error, Error::DatabaseBusy { .. }));
        let connection = Connection::open(&fixture.path).expect("source should remain readable");
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM alpha WHERE id = 1000", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("concurrent row count should be readable"),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn target_replacement_after_backup_aborts_before_swap() {
        let fixture = Fixture::new("target-replacement.db");
        let displaced = fixture.path.with_extension("displaced");
        let hooks = ReplaceTargetAfterBackupHooks {
            source: fixture.path.clone(),
            displaced: displaced.clone(),
        };

        let error = run_with(
            &fixture,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &hooks,
        )
        .expect_err("replaced swap target should abort");

        assert!(matches!(error, Error::DatabaseBusy { .. }));
        assert_eq!(
            fs::read(&fixture.path).expect("replacement should remain readable"),
            b"attacker replacement"
        );
        assert_eq!(
            fs::read(displaced).expect("original source should remain readable"),
            fixture.original_bytes
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn anchored_rename_stays_in_pinned_parent_during_ancestor_swap() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let live_directory = directory.path().join("live");
        let displaced_directory = directory.path().join("displaced");
        fs::create_dir(&live_directory).expect("database directory should be created");
        let database_path = live_directory.join("opencode.db");
        let temporary_path = live_directory.join("compacted.db");
        fs::write(&database_path, b"original").expect("database fixture should be created");
        fs::write(&temporary_path, b"compacted").expect("temporary fixture should be created");
        let anchor =
            crate::db::anchor_for_holder_scan(&database_path).expect("database should be anchored");

        fs::rename(&live_directory, &displaced_directory)
            .expect("database directory should be displaced");
        fs::create_dir(&live_directory).expect("attacker directory should be created");
        fs::write(&database_path, b"attacker database")
            .expect("attacker database should be created");
        fs::write(&temporary_path, b"attacker temporary")
            .expect("attacker temporary should be created");

        anchor
            .rename_sibling_over_source(&temporary_path)
            .expect("anchored rename should succeed");

        assert_eq!(
            fs::read(displaced_directory.join("opencode.db"))
                .expect("pinned database should remain readable"),
            b"compacted"
        );
        assert_eq!(
            fs::read(&database_path).expect("attacker database should remain readable"),
            b"attacker database"
        );
    }

    #[test]
    fn rename_failure_under_skip_backup_keeps_canonical_original_and_cleans_link() {
        let fixture = Fixture::new("rename-failure.db");
        let operations = FailingRenameOperations::new();
        let source_hash = content_hash(&fixture.path);
        let error = run_with(
            &fixture,
            VacuumIntoOptions { skip_backup: true },
            &operations,
            &NoopHooks,
        )
        .expect_err("injected rename should fail");

        assert!(matches!(error, Error::Io { .. }));
        assert!(operations.hard_link_called.load(Ordering::SeqCst));
        assert!(
            operations
                .canonical_existed_at_rename
                .load(Ordering::SeqCst)
        );
        assert!(
            !operations
                .rename_source_was_canonical
                .load(Ordering::SeqCst)
        );
        assert_eq!(content_hash(&fixture.path), source_hash);
        assert_eq!(
            directory_names(fixture.path.parent().expect("fixture parent")),
            vec!["rename-failure.db".to_owned()]
        );
    }

    #[test]
    fn rename_and_backup_cleanup_double_failure_returns_swap_rollback_failed() {
        let fixture = Fixture::new("rollback-failure.db");
        let operations = InjectedSwapFailures::new(true, true);

        let error = run_with(
            &fixture,
            VacuumIntoOptions { skip_backup: true },
            &operations,
            &NoopHooks,
        )
        .expect_err("rename and backup cleanup should both fail");

        assert_eq!(error.exit_code(), 11);
        let Error::SwapRollbackFailed {
            database_path,
            backup_path,
        } = error
        else {
            panic!("expected fatal swap rollback failure");
        };
        assert!(
            same_file_path(&database_path, &fixture.path),
            "rollback should name the fixture database: {} vs {}",
            database_path.display(),
            fixture.path.display()
        );
        assert!(backup_path.exists());
    }

    #[test]
    fn sidecar_unlink_failure_aborts_before_swap_and_cleans_temporary_output() {
        let fixture = Fixture::new("sidecar-unlink-failure.db");
        let source_hash = content_hash(&fixture.path);
        let error = run_with(
            &fixture,
            VacuumIntoOptions { skip_backup: true },
            &FailingSidecarRemovalOperations,
            &EmptySidecarHooks,
        )
        .expect_err("injected sidecar removal should fail");

        assert!(matches!(error, Error::Io { .. }));
        assert_eq!(content_hash(&fixture.path), source_hash);
        assert_eq!(
            directory_names(fixture.path.parent().expect("fixture parent")),
            vec![
                "sidecar-unlink-failure.db".to_owned(),
                "sidecar-unlink-failure.db-wal".to_owned(),
            ]
        );
    }

    #[test]
    fn unavailable_temporary_path_returns_typed_io_without_partial_output() {
        let fixture = Fixture::new("unwritable.db");
        let timestamp = "20260821T123456Z";
        let temporary = fixture
            .path
            .with_file_name(format!("unwritable.db.oc-clean-tmp-{timestamp}"));
        fs::create_dir(&temporary).expect("blocking directory should be created");
        let source_hash = content_hash(&fixture.path);
        let error = run_with(
            &fixture,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &FixedTimestampHooks(timestamp),
        )
        .expect_err("unavailable temporary path should fail");

        assert!(matches!(error, Error::Io { .. }));
        assert_eq!(content_hash(&fixture.path), source_hash);
        assert!(temporary.is_dir());
        assert_eq!(
            directory_names(fixture.path.parent().expect("fixture parent")),
            vec![
                "unwritable.db".to_owned(),
                format!("unwritable.db.oc-clean-tmp-{timestamp}"),
            ]
        );
    }

    #[test]
    fn post_swap_verification_failure_restores_original_from_recovery_link() {
        let fixture = Fixture::new("post-swap-failure.db");
        let source_hash = content_hash(&fixture.path);
        let error = run_with(
            &fixture,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &CorruptAfterRenameHooks,
        )
        .expect_err("post-swap corruption should fail verification");

        assert!(matches!(
            error,
            Error::IntegrityCheckFailed { .. } | Error::Sqlite { .. }
        ));
        assert_eq!(content_hash(&fixture.path), source_hash);
        assert_eq!(
            directory_names(fixture.path.parent().expect("fixture parent")),
            vec!["post-swap-failure.db".to_owned()]
        );
        let connection = Connection::open(&fixture.path).expect("restored source should open");
        assert_eq!(pragma_i64(&connection, "user_version"), USER_VERSION);
    }

    #[test]
    fn stale_same_connection_data_version_aborts_before_swap() {
        let fixture = Fixture::new("data-version.db");
        let source_hash = content_hash(&fixture.path);
        let (database, data_version) = fixture.locked();
        let error = vacuum_into_with_runtime(
            database,
            &fixture.path,
            data_version + 1,
            VacuumIntoOptions::default(),
            &SystemFileOperations,
            &NoopHooks,
        )
        .expect_err("stale data_version should abort");

        assert!(matches!(error, Error::DatabaseBusy { .. }));
        assert_eq!(content_hash(&fixture.path), source_hash);
        assert!(
            !directory_names(fixture.path.parent().expect("fixture parent"))
                .iter()
                .any(|name| name.contains("oc-clean-tmp"))
        );
    }

    #[test]
    fn auto_vacuum_is_verified_without_an_output_setting_statement() {
        let source = include_str!("vacuum_into.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source should precede test module");
        let pragma_assignment = ["PRAGMA auto", "_vacuum ="].concat();
        let pragma_update = ["pragma_update(None, \"auto", "_vacuum\""].concat();
        assert!(!source.contains(&pragma_assignment));
        assert!(!source.contains(&pragma_update));
    }
}
