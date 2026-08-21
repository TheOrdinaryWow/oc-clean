use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tempfile::TempDir;

#[path = "fixture/database.rs"]
mod database;
#[cfg(feature = "bench-large")]
#[path = "fixture/large.rs"]
#[allow(dead_code)]
mod large;
#[path = "fixture/trees.rs"]
mod trees;

#[cfg(feature = "bench-large")]
#[allow(unused_imports)]
pub use large::{LargeFixtureError, LargeFixtureReport};

pub type FixtureResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const SCHEMA: &str = include_str!("schema.sql");
const FRESH_MARKER: &str = "-- @shape fresh";
const END_MARKER: &str = "-- @end";
pub(super) const BASE_TIME_MS: i64 = 1_800_000_000_000;

pub const TABLES: &[&str] = &[
    "account",
    "account_state",
    "control_account",
    "credential",
    "data_migration",
    "event",
    "event_sequence",
    "message",
    "migration",
    "part",
    "permission",
    "project",
    "project_directory",
    "session",
    "session_context_epoch",
    "session_input",
    "session_message",
    "session_share",
    "todo",
    "workspace",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SchemaShape {
    #[default]
    Upgraded,
    Fresh,
}

#[derive(Clone, Debug)]
pub struct FixtureConfig {
    pub shape: SchemaShape,
    pub project_count: usize,
    pub session_count: usize,
    pub messages_per_session: usize,
    pub parts_per_message: usize,
    pub blob_size_per_part: usize,
    pub sub_session_depth: usize,
    pub sub_session_fan_out: usize,
    pub orphan_event_count: usize,
    pub dangling_parent_session_count: usize,
    pub archived_session_count: usize,
    pub time_span_ms: i64,
    pub orphan_storage_file_count: usize,
    pub orphan_snapshot_dir_count: usize,
    #[cfg(feature = "bench-large")]
    pub target_size_bytes: Option<u64>,
}

impl Default for FixtureConfig {
    fn default() -> Self {
        Self {
            shape: SchemaShape::Upgraded,
            project_count: 1,
            session_count: 1,
            messages_per_session: 1,
            parts_per_message: 1,
            blob_size_per_part: 0,
            sub_session_depth: 0,
            sub_session_fan_out: 0,
            orphan_event_count: 0,
            dangling_parent_session_count: 0,
            archived_session_count: 0,
            time_span_ms: 86_400_000,
            orphan_storage_file_count: 0,
            orphan_snapshot_dir_count: 0,
            #[cfg(feature = "bench-large")]
            target_size_bytes: None,
        }
    }
}

impl FixtureConfig {
    #[cfg(feature = "bench-large")]
    pub fn bench_large(target_size_bytes: u64) -> Self {
        Self {
            project_count: 32,
            session_count: 0,
            messages_per_session: 20,
            parts_per_message: 4,
            target_size_bytes: Some(target_size_bytes),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn expected_session_count(&self) -> usize {
        let (descendants, _) =
            (0..self.sub_session_depth).fold((0_usize, self.session_count), |(total, level), _| {
                let next = level.saturating_mul(self.sub_session_fan_out);
                (total.saturating_add(next), next)
            });
        self.session_count
            .saturating_add(descendants)
            .saturating_add(self.dangling_parent_session_count)
    }
}

pub struct Fixture {
    pub temp_dir: TempDir,
    pub database_path: PathBuf,
    pub storage_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub session_ids: Vec<String>,
    pub orphan_storage_files: Vec<PathBuf>,
    pub orphan_snapshot_dirs: Vec<PathBuf>,
    #[cfg(feature = "bench-large")]
    #[allow(dead_code)]
    large_report: Option<LargeFixtureReport>,
}

impl Fixture {
    pub fn build(config: &FixtureConfig) -> FixtureResult<Self> {
        validate_config(config)?;
        let temp_dir = tempfile::tempdir()?;
        let database_path = temp_dir.path().join("opencode.db");
        let storage_dir = temp_dir.path().join("storage");
        let snapshot_dir = temp_dir.path().join("snapshot");
        #[cfg(feature = "bench-large")]
        let large_report = if let Some(target_size_bytes) = config.target_size_bytes {
            large::ensure_capacity(temp_dir.path(), target_size_bytes)?;
            Some(large::populate(&database_path, config, target_size_bytes)?)
        } else {
            None
        };
        #[cfg(feature = "bench-large")]
        if large_report.is_some() {
            return Ok(Self {
                temp_dir,
                database_path,
                storage_dir,
                snapshot_dir,
                session_ids: Vec::new(),
                orphan_storage_files: Vec::new(),
                orphan_snapshot_dirs: Vec::new(),
                large_report,
            });
        }
        let mut connection = Connection::open(&database_path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.execute_batch(schema_ddl(config.shape))?;
        let session_ids = database::populate(&mut connection, config)?;
        let orphan_storage_files = trees::create_storage(&storage_dir, &session_ids, config)?;
        let orphan_snapshot_dirs = trees::create_snapshots(&snapshot_dir, config)?;
        Ok(Self {
            temp_dir,
            database_path,
            storage_dir,
            snapshot_dir,
            session_ids,
            orphan_storage_files,
            orphan_snapshot_dirs,
            #[cfg(feature = "bench-large")]
            large_report: None,
        })
    }

    pub fn connect(&self) -> rusqlite::Result<Connection> {
        let connection = Connection::open(&self.database_path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.temp_dir.path()
    }

    #[cfg(feature = "bench-large")]
    #[must_use]
    #[allow(dead_code)]
    pub fn large_report(&self) -> Option<&LargeFixtureReport> {
        self.large_report.as_ref()
    }
}

pub fn assert_schema_matches_committed_ddl(
    connection: &Connection,
    shape: SchemaShape,
) -> FixtureResult<()> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(schema_ddl(shape))?;
    if schema_objects(connection)? == schema_objects(&expected)? {
        Ok(())
    } else {
        Err(io::Error::other("sqlite_master differs from committed schema.sql").into())
    }
}

fn schema_ddl(shape: SchemaShape) -> &'static str {
    let fresh_start = SCHEMA
        .find(FRESH_MARKER)
        .expect("committed schema must contain fresh marker");
    let end = SCHEMA
        .find(END_MARKER)
        .expect("committed schema must contain end marker");
    match shape {
        SchemaShape::Upgraded => &SCHEMA[..fresh_start],
        SchemaShape::Fresh => &SCHEMA[fresh_start + FRESH_MARKER.len()..end],
    }
}

fn schema_objects(connection: &Connection) -> rusqlite::Result<Vec<(String, String, String)>> {
    let mut statement = connection.prepare(
        "SELECT type, name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL ORDER BY type, name",
    )?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect()
}

fn validate_config(config: &FixtureConfig) -> FixtureResult<()> {
    if config.project_count == 0 && config.expected_session_count() > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "project_count must be positive when sessions are requested",
        )
        .into());
    }
    if config.archived_session_count > config.expected_session_count() || config.time_span_ms < 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid fixture bounds").into());
    }
    Ok(())
}
