use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::fs;
use std::fs::File;
#[cfg(unix)]
use std::io::{self, Seek};
use std::marker::PhantomData;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, InterruptHandle, OpenFlags};
#[cfg(unix)]
use rustix::fs::{AtFlags, Mode, OFlags, linkat, openat, renameat, unlinkat};

use crate::error::Error;
use crate::paths::Target;
#[cfg(unix)]
use crate::safety::holders::open_database_target;
#[cfg(not(unix))]
use crate::safety::holders::resolve_database_target;

pub mod schema;

const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);
const DEFAULT_CACHE_SIZE: i32 = -64_000;
const OPENCODE_SCHEMA: &str = include_str!("opencode_schema.sql");
const FRESH_SCHEMA_MARKER: &str = "-- @shape fresh";
const SCHEMA_END_MARKER: &str = "-- @end";
static G_LINK_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static DATABASE_ANCHORS: OnceLock<Mutex<HashMap<PathBuf, Arc<AnchoredDatabaseFile>>>> =
    OnceLock::new();

/// Capabilities discovered for the active SQLite build and database filesystem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub octet_length: bool,
    pub dbstat: bool,
    pub hard_links: bool,
    pub sqlite_version: String,
}

/// Connection-local settings applied whenever a database is opened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionOptions {
    pub busy_timeout: Duration,
    /// SQLite page-cache size. Negative values are kibibytes rather than pages.
    pub cache_size: i32,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            cache_size: DEFAULT_CACHE_SIZE,
        }
    }
}

/// Read-only access marker. Connections with this marker have no write-lock API.
#[derive(Debug)]
pub struct ReadOnly;

/// Read-write access marker. Connections with this marker may acquire an exclusive lock.
#[derive(Debug)]
pub struct ReadWrite;

/// `(size, mtime seconds, mtime nanoseconds, device, inode or file index)`.
pub type FileIdentity = (u64, i64, i64, u64, u64);

/// An opened database file paired with the resolved path naming the same file.
#[derive(Debug)]
pub(crate) struct AnchoredDatabaseFile {
    descriptor: File,
    #[cfg(unix)]
    parent_descriptor: File,
    #[cfg(unix)]
    file_name: OsString,
    lexical_path: PathBuf,
    resolved_path: PathBuf,
}

/// A configured SQLite connection whose access mode is encoded in its type.
#[derive(Debug)]
pub struct DatabaseConnection<Access> {
    connection: Connection,
    capabilities: Capabilities,
    database_anchor: Option<Arc<AnchoredDatabaseFile>>,
    access: PhantomData<Access>,
}

/// A connection that can query SQLite while exposing no exclusive-lock operation.
///
/// ```compile_fail
/// use oc_clean::db::ReadOnlyConnection;
///
/// fn lock(connection: &ReadOnlyConnection) {
///     connection.acquire_exclusive_lock().unwrap();
/// }
/// ```
pub type ReadOnlyConnection = DatabaseConnection<ReadOnly>;
/// A connection that can query and mutate SQLite and acquire an exclusive lock.
pub type ReadWriteConnection = DatabaseConnection<ReadWrite>;

/// Opens an existing file database with SQLite's read-only flag or creates a fresh in-memory
/// database whose type exposes only read operations.
///
/// # Errors
///
/// Returns [`Error::NotFound`] when a file target is absent or an infrastructure error when
/// opening, initializing, or configuring SQLite fails.
pub fn open_read_only(
    target: &Target,
    options: ConnectionOptions,
) -> Result<ReadOnlyConnection, Error> {
    match target {
        Target::File(path) => open_file(
            path,
            options,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            "opening database read-only",
            AnchorRetention::Retain,
            || {},
        ),
        Target::Memory => {
            let connection = fresh_memory_connection()?;
            finish_open(connection, None, options)
        }
    }
}

/// Opens an existing database with read-write access.
///
/// # Errors
///
/// Returns [`Error::NotFound`] when a file target is absent or an infrastructure error when
/// opening, configuring, or probing the database fails.
pub fn open_read_write(
    target: &Target,
    options: ConnectionOptions,
) -> Result<ReadWriteConnection, Error> {
    match target {
        Target::File(path) => open_file(
            path,
            options,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            "opening database read-write",
            AnchorRetention::Consume,
            || {},
        ),
        Target::Memory => {
            let connection = fresh_memory_connection()?;
            finish_open(connection, None, options)
        }
    }
}

#[derive(Clone, Copy)]
enum AnchorRetention {
    Retain,
    Consume,
}

fn open_file<Access>(
    path: &Path,
    options: ConnectionOptions,
    flags: OpenFlags,
    context: &str,
    retention: AnchorRetention,
    before_open: impl FnOnce(),
) -> Result<DatabaseConnection<Access>, Error> {
    let anchor = database_anchor(path, retention)?;
    before_open();
    #[cfg(not(unix))]
    {
        anchor.ensure_path_matches(path, "database target changed before SQLite open")?;
        anchor
            .ensure_resolved_path_matches("resolved database target changed before SQLite open")?;
    }
    let expected_identity = anchor.identity()?;
    let connection = Connection::open_with_flags(anchor.sqlite_path(), flags)
        .map_err(|source| sqlite_error(context, source))?;
    anchor.ensure_identity(
        expected_identity,
        "anchored database changed during SQLite open",
    )?;
    finish_open(connection, Some(anchor), options)
}

#[cfg(all(test, unix))]
fn open_read_only_with_hook(
    target: &Target,
    options: ConnectionOptions,
    before_open: impl FnOnce(),
) -> Result<ReadOnlyConnection, Error> {
    match target {
        Target::File(path) => open_file(
            path,
            options,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            "opening database read-only",
            AnchorRetention::Retain,
            before_open,
        ),
        Target::Memory => open_read_only(target, options),
    }
}

impl AnchoredDatabaseFile {
    #[cfg(unix)]
    fn open(path: &Path) -> Result<Self, Error> {
        let target = open_database_target(path).map_err(|source| path_open_error(path, source))?;
        Ok(Self {
            descriptor: target.descriptor,
            parent_descriptor: target.parent_descriptor,
            file_name: target.file_name,
            lexical_path: path.to_path_buf(),
            resolved_path: target.resolved_path,
        })
    }

    #[cfg(windows)]
    fn open(path: &Path) -> Result<Self, Error> {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;

        let resolved_path =
            resolve_database_target(path).map_err(|source| path_open_error(path, source))?;
        // The anchor stays open for the whole command. Unix rename ignores open descriptors, but
        // Windows refuses to replace a file that any handle holds without delete sharing, which
        // would make this process block its own P20 swap. Full sharing restores the unix
        // behaviour; identity checks, not the handle, are what detect a swapped target.
        let descriptor = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&resolved_path)
            .map_err(|source| path_open_error(path, source))?;
        let anchor = Self {
            descriptor,
            lexical_path: path.to_path_buf(),
            resolved_path,
        };
        anchor.ensure_resolved_path_matches("database target changed while it was anchored")?;
        Ok(anchor)
    }

    #[cfg(not(any(unix, windows)))]
    fn open(path: &Path) -> Result<Self, Error> {
        let resolved_path =
            resolve_database_target(path).map_err(|source| path_open_error(path, source))?;
        let descriptor =
            File::open(&resolved_path).map_err(|source| path_open_error(path, source))?;
        let anchor = Self {
            descriptor,
            lexical_path: path.to_path_buf(),
            resolved_path,
        };
        anchor.ensure_resolved_path_matches("database target changed while it was anchored")?;
        Ok(anchor)
    }

    pub(crate) fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    #[cfg(unix)]
    pub(crate) fn sqlite_path(&self) -> PathBuf {
        descriptor_path(&self.descriptor)
    }

    #[cfg(not(unix))]
    pub(crate) fn sqlite_path(&self) -> PathBuf {
        self.resolved_path.clone()
    }

    #[cfg(unix)]
    pub(crate) fn inspection_path(&self) -> PathBuf {
        descriptor_path(&self.parent_descriptor).join(&self.file_name)
    }

    #[cfg(not(unix))]
    pub(crate) fn inspection_path(&self) -> PathBuf {
        self.resolved_path.clone()
    }

    #[cfg(unix)]
    pub(crate) fn sibling_path(&self, path: &Path) -> Result<PathBuf, Error> {
        let name = self.sibling_name(path)?;
        Ok(descriptor_path(&self.parent_descriptor).join(name))
    }

    #[cfg(not(unix))]
    #[expect(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "mirrors the fallible descriptor-anchored unix signature"
    )]
    pub(crate) fn sibling_path(&self, path: &Path) -> Result<PathBuf, Error> {
        Ok(path.to_path_buf())
    }

    #[cfg(unix)]
    pub(crate) fn hard_link_to(&self, destination: &Path) -> io::Result<()> {
        let destination = self.sibling_name_io(destination)?;
        linkat(
            rustix::fs::CWD,
            descriptor_path(&self.descriptor),
            &self.parent_descriptor,
            destination,
            AtFlags::SYMLINK_FOLLOW,
        )
        .map_err(Into::into)
    }

    #[cfg(unix)]
    pub(crate) fn copy_to_exclusive(&self, destination: &Path) -> io::Result<u64> {
        let destination = self.sibling_name_io(destination)?;
        let output = openat(
            &self.parent_descriptor,
            &destination,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut output = File::from(output);
        let mut input = self.descriptor.try_clone()?;
        input.rewind()?;
        let result = io::copy(&mut input, &mut output).and_then(|bytes| {
            output.set_permissions(input.metadata()?.permissions())?;
            Ok(bytes)
        });
        if result.is_err() {
            drop(output);
            let _ = unlinkat(&self.parent_descriptor, &destination, AtFlags::empty());
        }
        result
    }

    #[cfg(unix)]
    pub(crate) fn rename_sibling_over_source(&self, source: &Path) -> io::Result<()> {
        renameat(
            &self.parent_descriptor,
            self.sibling_name_io(source)?,
            &self.parent_descriptor,
            &self.file_name,
        )
        .map_err(Into::into)
    }

    #[cfg(unix)]
    pub(crate) fn remove_sibling(&self, path: &Path) -> io::Result<()> {
        unlinkat(
            &self.parent_descriptor,
            self.sibling_name_io(path)?,
            AtFlags::empty(),
        )
        .map_err(Into::into)
    }

    #[cfg(unix)]
    fn sibling_name(&self, path: &Path) -> Result<OsString, Error> {
        self.sibling_name_io(path)
            .map_err(|source| path_open_error(path, source))
    }

    #[cfg(unix)]
    fn sibling_name_io(&self, path: &Path) -> io::Result<OsString> {
        if path.parent() != self.resolved_path.parent() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is outside the anchored database directory",
                    path.display()
                ),
            ));
        }
        path.file_name().map(OsStr::to_owned).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no file name", path.display()),
            )
        })
    }

    pub(crate) fn identity(&self) -> Result<FileIdentity, Error> {
        file_identity_from_handle(&self.lexical_path, &self.descriptor)
    }

    pub(crate) fn ensure_path_matches(&self, path: &Path, reason: &str) -> Result<(), Error> {
        #[cfg(unix)]
        let candidate = open_database_target(path)
            .map_err(|source| path_open_error(path, source))?
            .descriptor;
        #[cfg(not(unix))]
        let candidate = File::open(path).map_err(|source| path_open_error(path, source))?;
        let candidate_identity = file_identity_from_handle(path, &candidate)?;
        self.ensure_same_file(candidate_identity, reason)
    }

    pub(crate) fn ensure_resolved_path_matches(&self, reason: &str) -> Result<(), Error> {
        #[cfg(unix)]
        let candidate_identity = {
            let candidate = File::from(
                openat(
                    &self.parent_descriptor,
                    &self.file_name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|source| Error::Io {
                    path: self.resolved_path.clone(),
                    source: source.into(),
                })?,
            );
            file_identity_from_handle(&self.resolved_path, &candidate)?
        };
        #[cfg(not(unix))]
        let candidate_identity = {
            let candidate = File::open(&self.resolved_path)
                .map_err(|source| path_open_error(&self.resolved_path, source))?;
            file_identity_from_handle(&self.resolved_path, &candidate)?
        };
        self.ensure_same_file(candidate_identity, reason)
    }

    pub(crate) fn ensure_identity(
        &self,
        expected: FileIdentity,
        reason: &str,
    ) -> Result<(), Error> {
        if self.identity()? == expected {
            Ok(())
        } else {
            Err(database_target_changed(reason))
        }
    }

    fn ensure_same_file(&self, candidate: FileIdentity, reason: &str) -> Result<(), Error> {
        if same_file(self.identity()?, candidate) {
            Ok(())
        } else {
            Err(database_target_changed(reason))
        }
    }
}

#[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
fn descriptor_path(descriptor: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", descriptor.as_raw_fd()))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn descriptor_path(descriptor: &File) -> PathBuf {
    PathBuf::from(format!("/dev/fd/{}", descriptor.as_raw_fd()))
}

pub(crate) fn anchor_for_holder_scan(path: &Path) -> Result<Arc<AnchoredDatabaseFile>, Error> {
    database_anchor(path, AnchorRetention::Retain)
}

fn database_anchor(
    path: &Path,
    retention: AnchorRetention,
) -> Result<Arc<AnchoredDatabaseFile>, Error> {
    let anchors = DATABASE_ANCHORS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut anchors = anchors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let existing = match retention {
        AnchorRetention::Retain => anchors.get(path).cloned(),
        AnchorRetention::Consume => anchors.remove(path),
    };
    drop(anchors);

    if let Some(anchor) = existing {
        anchor.ensure_path_matches(path, "database target changed between safety checks")?;
        return Ok(anchor);
    }

    let anchor = Arc::new(AnchoredDatabaseFile::open(path)?);
    if matches!(retention, AnchorRetention::Retain) {
        let mut anchors = DATABASE_ANCHORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let retained = anchors
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::clone(&anchor));
        retained.ensure_path_matches(path, "database target changed while retaining anchor")?;
        return Ok(Arc::clone(retained));
    }
    Ok(anchor)
}

fn same_file(left: FileIdentity, right: FileIdentity) -> bool {
    left.3 == right.3 && left.4 == right.4
}

fn database_target_changed(reason: &str) -> Error {
    Error::DatabaseBusy {
        holders: vec![reason.to_owned()],
    }
}

fn path_open_error(path: &Path, source: std::io::Error) -> Error {
    if source.kind() == std::io::ErrorKind::NotFound {
        Error::NotFound {
            path: path.to_path_buf(),
        }
    } else {
        Error::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

fn fresh_memory_connection() -> Result<Connection, Error> {
    let connection = Connection::open_in_memory()
        .map_err(|source| sqlite_error("opening in-memory database", source))?;
    let (_, fresh_schema) = OPENCODE_SCHEMA
        .split_once(FRESH_SCHEMA_MARKER)
        .ok_or_else(|| Error::InvalidArgument {
            argument: "embedded OpenCode schema".to_owned(),
            reason: "fresh schema marker is missing".to_owned(),
        })?;
    let (fresh_schema, _) =
        fresh_schema
            .split_once(SCHEMA_END_MARKER)
            .ok_or_else(|| Error::InvalidArgument {
                argument: "embedded OpenCode schema".to_owned(),
                reason: "schema end marker is missing".to_owned(),
            })?;
    connection
        .execute_batch(fresh_schema)
        .map_err(|source| sqlite_error("initializing in-memory database schema", source))?;
    Ok(connection)
}

impl<Access> DatabaseConnection<Access> {
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.connection
    }

    #[must_use]
    pub const fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Returns a thread-safe handle for interrupting work on this connection.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.connection.get_interrupt_handle()
    }

    /// Reads `PRAGMA data_version` for comparisons made on this same connection only.
    ///
    /// Fresh connections can report the same value on either side of an external write. Use
    /// [`Self::file_identity`] across close/reopen boundaries.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when SQLite cannot read the pragma.
    pub fn data_version(&self) -> Result<i64, Error> {
        self.connection
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .map_err(|source| sqlite_error("reading PRAGMA data_version", source))
    }

    /// Reads stable file metadata for external-modification checks across connection lifetimes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArgument`] for an in-memory database and [`Error::Io`] when file
    /// metadata cannot be read.
    pub fn file_identity(&self) -> Result<FileIdentity, Error> {
        let anchor = self
            .database_anchor
            .as_deref()
            .ok_or_else(|| Error::InvalidArgument {
                argument: ":memory:".to_owned(),
                reason: "file identity requires a file-backed database".to_owned(),
            })?;
        anchor.ensure_resolved_path_matches("database target changed while connection was open")?;
        anchor.identity()
    }

    pub(crate) fn database_anchor(&self) -> Result<Arc<AnchoredDatabaseFile>, Error> {
        self.database_anchor
            .clone()
            .ok_or_else(|| Error::InvalidArgument {
                argument: ":memory:".to_owned(),
                reason: "database anchoring requires a file-backed database".to_owned(),
            })
    }
}

impl DatabaseConnection<ReadWrite> {
    /// Switches SQLite to exclusive locking mode and immediately acquires the file lock.
    ///
    /// The connection retains the exclusive lock until it closes or its locking mode changes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DatabaseBusy`] when another connection prevents acquisition, or
    /// [`Error::Sqlite`] for another SQLite failure.
    pub fn acquire_exclusive_lock(&self) -> Result<(), Error> {
        self.connection
            .pragma_update(None, "locking_mode", "EXCLUSIVE")
            .map_err(|source| sqlite_error("setting PRAGMA locking_mode=EXCLUSIVE", source))?;
        self.connection
            .execute_batch("BEGIN IMMEDIATE; COMMIT;")
            .map_err(|source| sqlite_error("acquiring exclusive database lock", source))
    }
}

fn finish_open<Access>(
    connection: Connection,
    database_anchor: Option<Arc<AnchoredDatabaseFile>>,
    options: ConnectionOptions,
) -> Result<DatabaseConnection<Access>, Error> {
    apply_pragmas(&connection, options)?;
    let capabilities = probe_capabilities(&connection, database_anchor.as_deref())?;
    Ok(DatabaseConnection {
        connection,
        capabilities,
        database_anchor,
        access: PhantomData,
    })
}

fn apply_pragmas(connection: &Connection, options: ConnectionOptions) -> Result<(), Error> {
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(|source| sqlite_error("enabling PRAGMA foreign_keys", source))?;
    connection
        .busy_timeout(options.busy_timeout)
        .map_err(|source| sqlite_error("setting PRAGMA busy_timeout", source))?;
    connection
        .pragma_update(None, "temp_store", "MEMORY")
        .map_err(|source| sqlite_error("setting PRAGMA temp_store", source))?;
    connection
        .pragma_update(None, "cache_size", options.cache_size)
        .map_err(|source| sqlite_error("setting PRAGMA cache_size", source))
}

fn probe_capabilities(
    connection: &Connection,
    database_anchor: Option<&AnchoredDatabaseFile>,
) -> Result<Capabilities, Error> {
    let octet_length = connection
        .query_row("SELECT octet_length('x')", [], |row| row.get::<_, i64>(0))
        .is_ok();
    let dbstat = connection
        .prepare("SELECT name FROM dbstat LIMIT 1")
        .and_then(|mut statement| statement.exists([]))
        .is_ok();
    let sqlite_version = connection
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .map_err(|source| sqlite_error("probing SQLite version", source))?;
    let hard_links = match database_anchor {
        Some(anchor) => probe_hard_links(anchor)?,
        None => false,
    };

    Ok(Capabilities {
        octet_length,
        dbstat,
        hard_links,
        sqlite_version,
    })
}

fn probe_hard_links(anchor: &AnchoredDatabaseFile) -> Result<bool, Error> {
    let database_path = anchor.resolved_path();
    let Some(parent) = database_path.parent() else {
        return Ok(false);
    };
    let sequence = G_LINK_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let probe_path = parent.join(format!(
        ".oc-clean-link-probe-{}-{sequence}",
        std::process::id()
    ));
    #[cfg(unix)]
    let link_result = anchor.hard_link_to(&probe_path);
    #[cfg(not(unix))]
    let link_result = std::fs::hard_link(database_path, &probe_path);
    if link_result.is_err() {
        return Ok(false);
    }
    #[cfg(unix)]
    let remove_result = anchor.remove_sibling(&probe_path);
    #[cfg(not(unix))]
    let remove_result = std::fs::remove_file(&probe_path);
    remove_result.map_err(|source| Error::Io {
        path: probe_path,
        source,
    })?;
    Ok(true)
}

pub(crate) fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    if matches!(
        source.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    ) {
        Error::DatabaseBusy {
            holders: Vec::new(),
        }
    } else {
        Error::Sqlite {
            context: context.to_owned(),
            source,
        }
    }
}

#[cfg(unix)]
fn file_identity_from_handle(path: &Path, handle: &File) -> Result<FileIdentity, Error> {
    use std::os::unix::fs::MetadataExt;

    let metadata = handle.metadata().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((
        metadata.size(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.dev(),
        metadata.ino(),
    ))
}

/// Reads Windows file identity straight from an open handle.
///
/// `std::os::windows::fs::MetadataExt::file_index` is still unstable, so the identity comes from
/// `GetFileInformationByHandle`, which is stable Win32 and also supplies the volume serial number
/// that [`same_file`] needs to tell apart identically indexed files on different volumes.
#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "GetFileInformationByHandle is the stable Win32 source of volume and file index identity"
)]
fn file_identity_from_handle(path: &Path, handle: &File) -> Result<FileIdentity, Error> {
    use std::io;
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    const WINDOWS_TO_UNIX_SECONDS: u64 = 11_644_473_600;
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const NANOS_PER_TICK: u64 = 100;

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is a live, caller-owned file handle that outlives this call, and
    // `information` is a writable record of exactly the size the API documents.
    unsafe { GetFileInformationByHandle(HANDLE(handle.as_raw_handle()), &raw mut information) }
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source: io::Error::other(format!("GetFileInformationByHandle failed: {source}")),
        })?;

    let modified = (u64::from(information.ftLastWriteTime.dwHighDateTime) << 32)
        | u64::from(information.ftLastWriteTime.dwLowDateTime);
    let seconds = modified
        .checked_div(TICKS_PER_SECOND)
        .and_then(|value| value.checked_sub(WINDOWS_TO_UNIX_SECONDS))
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| Error::Io {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Windows modification time",
            ),
        })?;
    let nanoseconds = i64::try_from((modified % TICKS_PER_SECOND) * NANOS_PER_TICK)
        .expect("subsecond Windows timestamp always fits i64");
    let size = (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow);
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);

    Ok((
        size,
        seconds,
        nanoseconds,
        u64::from(information.dwVolumeSerialNumber),
        file_index,
    ))
}

#[cfg(not(any(unix, windows)))]
fn file_identity_from_handle(path: &Path, _handle: &File) -> Result<FileIdentity, Error> {
    Err(Error::UnsupportedPlatform {
        platform: format!("{} ({})", std::env::consts::OS, path.display()),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    fn create_database(path: &Path, marker: &str) {
        let connection = Connection::open(path).expect("fixture database should open");
        connection
            .execute("CREATE TABLE marker (value TEXT NOT NULL)", [])
            .expect("marker table should be created");
        connection
            .execute("INSERT INTO marker (value) VALUES (?1)", [marker])
            .expect("marker row should be inserted");
    }

    #[test]
    fn symlink_retarget_between_resolution_and_open_uses_pinned_database() {
        let directory = TempDir::new().expect("temporary directory should be created");
        let original = directory.path().join("original.db");
        let replacement = directory.path().join("replacement.db");
        let link = directory.path().join("opencode.db");
        create_database(&original, "original");
        create_database(&replacement, "replacement");
        symlink("original.db", &link).expect("database symlink should be created");

        let connection = open_read_only_with_hook(
            &Target::File(link.clone()),
            ConnectionOptions::default(),
            || {
                std::fs::remove_file(&link).expect("old database symlink should be removed");
                symlink("replacement.db", &link)
                    .expect("replacement database symlink should be created");
            },
        )
        .expect("SQLite should open the descriptor-pinned database");

        let marker = connection
            .connection()
            .query_row("SELECT value FROM marker", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("marker should be readable");
        assert_eq!(marker, "original");
    }
}
