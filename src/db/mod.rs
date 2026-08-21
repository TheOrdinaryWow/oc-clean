use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, InterruptHandle, OpenFlags};

use crate::error::Error;
use crate::paths::Target;
use crate::safety::holders::resolve_database_target;

pub mod schema;

const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);
const DEFAULT_CACHE_SIZE: i32 = -64_000;
const OPENCODE_SCHEMA: &str = include_str!("opencode_schema.sql");
const FRESH_SCHEMA_MARKER: &str = "-- @shape fresh";
const SCHEMA_END_MARKER: &str = "-- @end";
static G_LINK_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

/// `(size, mtime seconds, mtime nanoseconds, inode or file index)`.
pub type FileIdentity = (u64, i64, i64, u64);

/// A configured SQLite connection whose access mode is encoded in its type.
#[derive(Debug)]
pub struct DatabaseConnection<Access> {
    connection: Connection,
    capabilities: Capabilities,
    database_path: Option<PathBuf>,
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
        Target::File(path) => {
            ensure_exists(path)?;
            let connection = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|source| sqlite_error("opening database read-only", source))?;
            finish_open(connection, Some(path.clone()), options)
        }
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
        Target::File(path) => {
            ensure_exists(path)?;
            let connection = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|source| sqlite_error("opening database read-write", source))?;
            finish_open(connection, Some(path.clone()), options)
        }
        Target::Memory => {
            let connection = fresh_memory_connection()?;
            finish_open(connection, None, options)
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
        let path = self
            .database_path
            .as_deref()
            .ok_or_else(|| Error::InvalidArgument {
                argument: ":memory:".to_owned(),
                reason: "file identity requires a file-backed database".to_owned(),
            })?;
        file_identity(path)
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
    database_path: Option<PathBuf>,
    options: ConnectionOptions,
) -> Result<DatabaseConnection<Access>, Error> {
    apply_pragmas(&connection, options)?;
    let capabilities = probe_capabilities(&connection, database_path.as_deref())?;
    Ok(DatabaseConnection {
        connection,
        capabilities,
        database_path,
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
    database_path: Option<&Path>,
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
    let hard_links = match database_path {
        Some(path) => probe_hard_links(path)?,
        None => false,
    };

    Ok(Capabilities {
        octet_length,
        dbstat,
        hard_links,
        sqlite_version,
    })
}

fn probe_hard_links(database_path: &Path) -> Result<bool, Error> {
    let Some(parent) = database_path.parent() else {
        return Ok(false);
    };
    let sequence = G_LINK_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let probe_path = parent.join(format!(
        ".oc-clean-link-probe-{}-{sequence}",
        std::process::id()
    ));
    if fs::hard_link(database_path, &probe_path).is_err() {
        return Ok(false);
    }
    fs::remove_file(&probe_path).map_err(|source| Error::Io {
        path: probe_path,
        source,
    })?;
    Ok(true)
}

fn ensure_exists(path: &Path) -> Result<(), Error> {
    match resolve_database_target(path) {
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
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
fn file_identity(path: &Path) -> Result<FileIdentity, Error> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((
        metadata.size(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ino(),
    ))
}

#[cfg(windows)]
fn file_identity(path: &Path) -> Result<FileIdentity, Error> {
    use std::io;
    use std::os::windows::fs::MetadataExt;

    const WINDOWS_TO_UNIX_SECONDS: u64 = 11_644_473_600;
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const NANOS_PER_TICK: u64 = 100;

    let metadata = fs::metadata(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let modified = metadata.last_write_time();
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
    let file_index = metadata.file_index().ok_or_else(|| Error::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::Unsupported, "file index is unavailable"),
    })?;
    Ok((metadata.file_size(), seconds, nanoseconds, file_index))
}

#[cfg(not(any(unix, windows)))]
fn file_identity(path: &Path) -> Result<FileIdentity, Error> {
    Err(Error::UnsupportedPlatform {
        platform: format!("{} ({})", std::env::consts::OS, path.display()),
    })
}
