use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::db::{Capabilities, DatabaseConnection};
use crate::error::Error;

const DBSTAT_LABEL: &str = "SQLite dbstat page bytes (exact)";
const ESTIMATE_LABEL: &str =
    "estimate: payload bytes for five blob-bearing tables using octet_length()";

/// File-level SQLite page accounting and separate sidecar sizes.
#[derive(Clone, Debug, PartialEq)]
pub struct FileSpace {
    pub page_count: u32,
    pub freelist_count: u32,
    pub page_size: u32,
    pub total_bytes: u64,
    pub live_bytes: u64,
    pub freelist_bytes: u64,
    pub freelist_percent: f64,
    pub wal_bytes: Option<u64>,
    pub shm_bytes: Option<u64>,
}

/// How per-object byte counts were collected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountingMethod {
    Dbstat,
    OctetLengthEstimate,
}

/// The SQLite object represented by a per-object byte count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    Table,
    Index,
    Schema,
}

/// Space attributed to one SQLite table, index, or schema b-tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectSpace {
    pub name: String,
    pub kind: ObjectKind,
    pub bytes: u64,
}

/// Per-object accounting with an explicit accuracy label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectSpaceReport {
    pub method: AccountingMethod,
    pub label: &'static str,
    pub entries: Vec<ObjectSpace>,
}

impl ObjectSpaceReport {
    /// Returns whether the report contains payload estimates rather than page-accurate sizes.
    #[must_use]
    pub const fn is_estimate(&self) -> bool {
        matches!(self.method, AccountingMethod::OctetLengthEstimate)
    }
}

/// Complete file-level and per-object space accounting.
#[derive(Clone, Debug, PartialEq)]
pub struct SpaceReport {
    pub file: FileSpace,
    pub objects: ObjectSpaceReport,
}

/// Analyzes SQLite file pages and per-object storage using the connection's capabilities.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when SQLite accounting queries fail, [`Error::Io`] when sidecar
/// metadata cannot be read, or [`Error::InvalidArgument`] for an in-memory database or a SQLite
/// build lacking both supported per-object accounting mechanisms.
pub fn analyze<Access>(database: &DatabaseConnection<Access>) -> Result<SpaceReport, Error> {
    analyze_with_capabilities(database, database.capabilities())
}

/// Analyzes space using an explicit capability snapshot.
///
/// This entry point supports callers that persist a capability decision and deterministic tests of
/// degraded SQLite builds.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when SQLite accounting queries fail, [`Error::Io`] when sidecar
/// metadata cannot be read, or [`Error::InvalidArgument`] for an in-memory database or a SQLite
/// build lacking both supported per-object accounting mechanisms.
pub fn analyze_with_capabilities<Access>(
    database: &DatabaseConnection<Access>,
    capabilities: &Capabilities,
) -> Result<SpaceReport, Error> {
    let connection = database.connection();
    let database_path = main_database_path(connection)?;
    let file = file_space(connection, &database_path)?;
    let objects = if capabilities.dbstat {
        dbstat_space(connection)?
    } else if capabilities.octet_length {
        estimated_space(connection)?
    } else {
        return Err(Error::InvalidArgument {
            argument: "SQLite capabilities".to_owned(),
            reason: "per-object accounting requires dbstat or octet_length()".to_owned(),
        });
    };

    Ok(SpaceReport { file, objects })
}

fn file_space(connection: &Connection, database_path: &Path) -> Result<FileSpace, Error> {
    let page_count = pragma_u32(connection, "page_count")?;
    let freelist_count = pragma_u32(connection, "freelist_count")?;
    let page_size = pragma_u32(connection, "page_size")?;
    let total_bytes = u64::from(page_count) * u64::from(page_size);
    let freelist_bytes = u64::from(freelist_count) * u64::from(page_size);
    let live_bytes = u64::from(page_count.saturating_sub(freelist_count)) * u64::from(page_size);
    let freelist_percent = if page_count == 0 {
        0.0
    } else {
        f64::from(freelist_count) * 100.0 / f64::from(page_count)
    };

    Ok(FileSpace {
        page_count,
        freelist_count,
        page_size,
        total_bytes,
        live_bytes,
        freelist_bytes,
        freelist_percent,
        wal_bytes: sidecar_bytes(database_path, "-wal")?,
        shm_bytes: sidecar_bytes(database_path, "-shm")?,
    })
}

fn pragma_u32(connection: &Connection, pragma: &str) -> Result<u32, Error> {
    connection
        .pragma_query_value(None, pragma, |row| row.get(0))
        .map_err(|source| sqlite_error(&format!("reading PRAGMA {pragma}"), source))
}

fn main_database_path(connection: &Connection) -> Result<PathBuf, Error> {
    let path = connection
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name = ?1",
            ["main"],
            |row| row.get::<_, String>(0),
        )
        .map_err(|source| sqlite_error("reading the main database path", source))?;
    if path.is_empty() {
        return Err(Error::InvalidArgument {
            argument: ":memory:".to_owned(),
            reason: "space accounting requires a file-backed database".to_owned(),
        });
    }
    Ok(PathBuf::from(path))
}

fn sidecar_bytes(database_path: &Path, suffix: &str) -> Result<Option<u64>, Error> {
    let mut sidecar = OsString::from(database_path.as_os_str());
    sidecar.push(suffix);
    let sidecar = PathBuf::from(sidecar);
    match fs::metadata(&sidecar) {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: sidecar,
            source,
        }),
    }
}

fn dbstat_space(connection: &Connection) -> Result<ObjectSpaceReport, Error> {
    let object_kinds = sqlite_object_kinds(connection)?;
    let mut statement = connection
        .prepare(
            "SELECT name, SUM(pgsize) FROM dbstat WHERE aggregate=TRUE GROUP BY name ORDER BY 2 DESC",
        )
        .map_err(|source| sqlite_error("preparing aggregate dbstat accounting", source))?;
    let mut entries = statement
        .query_map([], |row| {
            let name = row.get::<_, String>(0)?;
            let bytes = row.get::<_, i64>(1)?;
            let bytes = u64::try_from(bytes)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, bytes))?;
            let kind = object_kinds
                .get(name.as_str())
                .copied()
                .unwrap_or(ObjectKind::Schema);
            Ok(ObjectSpace { name, kind, bytes })
        })
        .map_err(|source| sqlite_error("querying aggregate dbstat accounting", source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error("reading aggregate dbstat accounting", source))?;
    sort_entries(&mut entries);

    Ok(ObjectSpaceReport {
        method: AccountingMethod::Dbstat,
        label: DBSTAT_LABEL,
        entries,
    })
}

fn sqlite_object_kinds(connection: &Connection) -> Result<HashMap<String, ObjectKind>, Error> {
    let mut statement = connection
        .prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table', 'index')")
        .map_err(|source| sqlite_error("preparing SQLite object classification", source))?;
    statement
        .query_map([], |row| {
            let kind = match row.get::<_, String>(1)?.as_str() {
                "table" => ObjectKind::Table,
                "index" => ObjectKind::Index,
                _ => unreachable!("sqlite_master query filters object types"),
            };
            Ok((row.get(0)?, kind))
        })
        .map_err(|source| sqlite_error("querying SQLite object classification", source))?
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(|source| sqlite_error("reading SQLite object classification", source))
}

fn estimated_space(connection: &Connection) -> Result<ObjectSpaceReport, Error> {
    let mut entries = vec![
        estimated_table(
            connection,
            "event",
            "SELECT COALESCE(SUM(octet_length(data)), 0) FROM event",
        )?,
        estimated_table(
            connection,
            "message",
            "SELECT COALESCE(SUM(octet_length(data)), 0) FROM message",
        )?,
        estimated_table(
            connection,
            "part",
            "SELECT COALESCE(SUM(octet_length(data)), 0) FROM part",
        )?,
        estimated_table(
            connection,
            "session_context_epoch",
            "SELECT COALESCE(SUM(octet_length(baseline) + octet_length(snapshot)), 0) FROM session_context_epoch",
        )?,
        estimated_table(
            connection,
            "session_message",
            "SELECT COALESCE(SUM(octet_length(data)), 0) FROM session_message",
        )?,
    ];
    sort_entries(&mut entries);

    Ok(ObjectSpaceReport {
        method: AccountingMethod::OctetLengthEstimate,
        label: ESTIMATE_LABEL,
        entries,
    })
}

fn estimated_table(
    connection: &Connection,
    table_name: &str,
    sql: &str,
) -> Result<ObjectSpace, Error> {
    let bytes = connection
        .query_row(sql, [], |row| row.get::<_, i64>(0))
        .map_err(|source| {
            sqlite_error(
                &format!("estimating payload bytes for `{table_name}`"),
                source,
            )
        })?;
    let bytes = u64::try_from(bytes).map_err(|_| {
        sqlite_error(
            &format!("reading estimated payload bytes for `{table_name}`"),
            rusqlite::Error::IntegralValueOutOfRange(0, bytes),
        )
    })?;
    Ok(ObjectSpace {
        name: table_name.to_owned(),
        kind: ObjectKind::Table,
        bytes,
    })
}

fn sort_entries(entries: &mut [ObjectSpace]) {
    entries.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.name.cmp(&right.name))
    });
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        context: context.to_owned(),
        source,
    }
}
