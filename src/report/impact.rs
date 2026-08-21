//! Read-only pre-flight impact calculation for future cleanup commands.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
use rusqlite::types::ValueRef;

use crate::analyze::{attribution, orphans as orphan_census, space};
use crate::cli::types::{Duration, Size};
use crate::db::DatabaseConnection;
use crate::error::Error;
use crate::paths::DerivedPaths;
use crate::select::orphans;
use crate::select::predicates::{self, CaseSensitivity, SessionIds, intersect_candidate_sets};
use crate::select::{retention, subtree};

/// Tables whose rows can be removed by session deletion, event cleanup, or project pruning.
pub const DELETION_TABLES: &[&str] = &[
    "event",
    "event_sequence",
    "message",
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

const SESSION_TABLES: &[TableSpec] = &[
    TableSpec::new("session", "id", &[]),
    TableSpec::new("message", "session_id", &["data"]),
    TableSpec::new("part", "session_id", &["data"]),
    TableSpec::new(
        "session_context_epoch",
        "session_id",
        &["baseline", "snapshot"],
    ),
    TableSpec::new("session_input", "session_id", &[]),
    TableSpec::new("session_message", "session_id", &["data"]),
    TableSpec::new("session_share", "session_id", &[]),
    TableSpec::new("todo", "session_id", &[]),
    TableSpec::new("event_sequence", "aggregate_id", &[]),
    TableSpec::new("event", "aggregate_id", &["data"]),
];

const PROJECT_TABLES: &[TableSpec] = &[
    TableSpec::new("project", "id", &[]),
    TableSpec::new("project_directory", "project_id", &[]),
    TableSpec::new("permission", "project_id", &[]),
    TableSpec::new("workspace", "project_id", &[]),
];

#[derive(Clone, Copy)]
struct TableSpec {
    table: &'static str,
    owner_column: &'static str,
    attributed_payload_columns: &'static [&'static str],
}

impl TableSpec {
    const fn new(
        table: &'static str,
        owner_column: &'static str,
        attributed_payload_columns: &'static [&'static str],
    ) -> Self {
        Self {
            table,
            owner_column,
            attributed_payload_columns,
        }
    }
}

/// One requested age predicate and the clock instant used to evaluate it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OlderThanSelection {
    pub age: Duration,
    pub now_ms: i64,
}

/// A project-path predicate whose result is based on project membership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSelection {
    pub path_or_glob: String,
    pub case_sensitivity: CaseSensitivity,
}

/// Predicate, retention, and orphan options used to calculate deletion impact.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImpactSelection {
    pub older_than: Option<OlderThanSelection>,
    pub archived: bool,
    pub project: Option<ProjectSelection>,
    pub larger_than: Option<Size>,
    pub keep_recent: u64,
    pub sweep_orphans: bool,
}

/// Project-selection context shown before confirmation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSelectionImpact {
    pub path_or_glob: String,
    pub matched_sessions: u64,
}

/// User-facing counts and byte projections for one cleanup selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImpactSummary {
    pub root_session_count: u64,
    pub total_session_count: u64,
    pub table_rows: BTreeMap<String, u64>,
    pub orphan_row_count: u64,
    pub storage_file_count: u64,
    pub snapshot_directory_count: u64,
    pub project_prune_count: u64,
    pub database_bytes: u64,
    pub filesystem_bytes: u64,
    pub total_bytes: u64,
    pub current_live_bytes: u64,
    pub estimated_post_vacuum_bytes: u64,
    pub project_selection: Option<ProjectSelectionImpact>,
}

impl ImpactSummary {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total_session_count == 0
            && self.orphan_row_count == 0
            && self.storage_file_count == 0
            && self.snapshot_directory_count == 0
            && self.project_prune_count == 0
    }
}

/// Exact identifiers and paths paired with the rendered summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Impact {
    pub summary: ImpactSummary,
    pub session_ids: SessionIds,
    pub orphan_event_aggregate_ids: BTreeSet<String>,
    pub project_ids: BTreeSet<String>,
    pub storage_files: BTreeSet<PathBuf>,
    pub snapshot_directories: BTreeSet<PathBuf>,
}

/// Adds the database byte classes attributable to a deletion selection.
///
/// The plain byte-count contract lets disk-headroom checks reuse the projection without depending
/// on selection predicates, filesystem paths, or rendering types.
#[must_use]
pub const fn total_attributable_bytes(
    session_payload_bytes: u64,
    row_metadata_bytes: u64,
    orphan_row_bytes: u64,
) -> u64 {
    session_payload_bytes
        .saturating_add(row_metadata_bytes)
        .saturating_add(orphan_row_bytes)
}

struct PlannedSelection {
    root_count: u64,
    session_ids: SessionIds,
    dangling_session_ids: SessionIds,
    orphan_event_aggregate_ids: BTreeSet<String>,
    project_selection: Option<ProjectSelectionImpact>,
    raw_orphans: orphans::RawOrphans,
}

struct DatabaseImpact {
    table_rows: BTreeMap<String, u64>,
    orphan_row_count: u64,
    project_ids: BTreeSet<String>,
    database_bytes: u64,
    current_live_bytes: u64,
}

struct AssetImpact {
    storage_files: BTreeSet<PathBuf>,
    snapshot_directories: BTreeSet<PathBuf>,
    filesystem_bytes: u64,
}

/// Computes a complete cleanup impact without mutating the database or filesystem.
///
/// # Errors
///
/// Returns an infrastructure error when a predicate, aggregation, space query, or filesystem
/// census cannot be completed.
pub fn summarize<Access>(
    database: &DatabaseConnection<Access>,
    paths: &DerivedPaths,
    selection: &ImpactSelection,
) -> Result<Impact, Error> {
    let planned = plan_selection(database, paths, selection)?;
    let database_impact = database_impact(database, paths, selection, &planned)?;
    let assets = asset_impact(paths, selection, &planned, &database_impact.project_ids)?;
    let total_bytes = database_impact
        .database_bytes
        .saturating_add(assets.filesystem_bytes);

    Ok(Impact {
        summary: ImpactSummary {
            root_session_count: planned.root_count,
            total_session_count: to_u64_len(planned.session_ids.len()),
            table_rows: database_impact.table_rows,
            orphan_row_count: database_impact.orphan_row_count,
            storage_file_count: to_u64_len(assets.storage_files.len()),
            snapshot_directory_count: to_u64_len(assets.snapshot_directories.len()),
            project_prune_count: to_u64_len(database_impact.project_ids.len()),
            database_bytes: database_impact.database_bytes,
            filesystem_bytes: assets.filesystem_bytes,
            total_bytes,
            current_live_bytes: database_impact.current_live_bytes,
            estimated_post_vacuum_bytes: database_impact
                .current_live_bytes
                .saturating_sub(database_impact.database_bytes),
            project_selection: planned.project_selection,
        },
        session_ids: planned.session_ids,
        orphan_event_aggregate_ids: planned.orphan_event_aggregate_ids,
        project_ids: database_impact.project_ids,
        storage_files: assets.storage_files,
        snapshot_directories: assets.snapshot_directories,
    })
}

/// Writes the stable human-readable dry-run and confirmation summary.
///
/// # Errors
///
/// Returns the underlying writer error when output cannot be completed.
pub fn write_human(summary: &ImpactSummary, output: &mut dyn Write) -> io::Result<()> {
    if summary.is_empty() {
        writeln!(output, "nothing to delete")?;
        return Ok(());
    }
    writeln!(output, "Cleanup impact (dry-run)")?;
    writeln!(output, "  Root sessions: {}", summary.root_session_count)?;
    writeln!(output, "  Total sessions: {}", summary.total_session_count)?;
    for (table, rows) in &summary.table_rows {
        writeln!(output, "  {table}: {rows} rows")?;
    }
    writeln!(output, "  Orphan rows: {}", summary.orphan_row_count)?;
    writeln!(output, "  Storage files: {}", summary.storage_file_count)?;
    writeln!(
        output,
        "  Snapshot directories: {}",
        summary.snapshot_directory_count
    )?;
    writeln!(output, "  Projects pruned: {}", summary.project_prune_count)?;
    writeln!(output, "  Total bytes: {}", summary.total_bytes)?;
    writeln!(
        output,
        "  Estimated post-VACUUM size: {}",
        summary.estimated_post_vacuum_bytes
    )?;
    if let Some(project) = &summary.project_selection {
        writeln!(
            output,
            "  Project `{}` selects ALL {} member sessions regardless of each session directory",
            project.path_or_glob, project.matched_sessions
        )?;
    }
    Ok(())
}

fn plan_selection<Access>(
    database: &DatabaseConnection<Access>,
    paths: &DerivedPaths,
    selection: &ImpactSelection,
) -> Result<PlannedSelection, Error> {
    let (candidate_sessions, project_selection) = candidate_roots(database, selection)?;
    let mut candidate_roots = selection_roots(database, &candidate_sessions)?;
    let retained = retention::compute(database, selection.keep_recent)?;
    candidate_roots.retain(|session_id| !retained.contains(session_id));
    let mut session_ids = subtree::expand(database, &candidate_roots)?;
    session_ids.retain(|session_id| !retained.contains(session_id));

    let raw_orphans = orphans::select(database, paths)?;
    let orphan_event_aggregate_ids = if selection.sweep_orphans {
        raw_orphans
            .event_aggregate_ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect()
    } else {
        BTreeSet::new()
    };
    let dangling_session_ids = if selection.sweep_orphans {
        raw_orphans
            .dangling_session_ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .filter(|id| !retained.contains(id))
            .collect()
    } else {
        SessionIds::new()
    };
    session_ids.extend(dangling_session_ids.iter().cloned());

    Ok(PlannedSelection {
        root_count: to_u64_len(candidate_roots.len()),
        session_ids,
        dangling_session_ids,
        orphan_event_aggregate_ids,
        project_selection,
        raw_orphans,
    })
}

fn database_impact<Access>(
    database: &DatabaseConnection<Access>,
    paths: &DerivedPaths,
    selection: &ImpactSelection,
    planned: &PlannedSelection,
) -> Result<DatabaseImpact, Error> {
    let session_payload_bytes = selected_session_payload_bytes(database, &planned.session_ids)?;
    let project_ids = projects_emptied_by(database, &planned.session_ids)?;
    let mut impact_by_table = DELETION_TABLES
        .iter()
        .map(|table| ((*table).to_owned(), TableImpact::default()))
        .collect();
    merge_table_impact(
        &mut impact_by_table,
        table_impact(database, SESSION_TABLES, &planned.session_ids)?,
    );
    merge_table_impact(
        &mut impact_by_table,
        table_impact(database, PROJECT_TABLES, &project_ids)?,
    );
    let row_metadata_bytes = impact_by_table
        .values()
        .fold(0_u64, |total, table| total.saturating_add(table.bytes));
    let orphan_rows = table_impact(
        database,
        &[
            TableSpec::new("event_sequence", "aggregate_id", &[]),
            TableSpec::new("event", "aggregate_id", &[]),
        ],
        &planned.orphan_event_aggregate_ids,
    )?;
    merge_table_impact(&mut impact_by_table, orphan_rows);
    let orphan_row_bytes = if selection.sweep_orphans {
        orphan_census::analyze(database, paths)?.orphan_events.bytes
    } else {
        0
    };
    let database_bytes =
        total_attributable_bytes(session_payload_bytes, row_metadata_bytes, orphan_row_bytes);
    let file_space = space::analyze(database)?.file;

    Ok(DatabaseImpact {
        table_rows: impact_by_table
            .into_iter()
            .map(|(table, impact)| (table, impact.rows))
            .collect(),
        orphan_row_count: to_u64_len(planned.orphan_event_aggregate_ids.len())
            .saturating_add(to_u64_len(planned.dangling_session_ids.len())),
        project_ids,
        database_bytes,
        current_live_bytes: file_space.live_bytes,
    })
}

fn selected_session_payload_bytes<Access>(
    database: &DatabaseConnection<Access>,
    session_ids: &SessionIds,
) -> Result<u64, Error> {
    let session_count = database
        .connection()
        .query_row("SELECT COUNT(*) FROM session", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|source| sqlite_error("counting sessions for impact attribution", source))?;
    let top_n = to_usize(
        session_count,
        "reading session count for impact attribution",
    )?;
    Ok(attribution::analyze(database, top_n)?
        .sessions
        .iter()
        .filter(|session| session_ids.contains(&session.session_id))
        .fold(0_u64, |total, session| {
            total.saturating_add(session.self_bytes)
        }))
}

fn asset_impact(
    paths: &DerivedPaths,
    selection: &ImpactSelection,
    planned: &PlannedSelection,
    project_ids: &BTreeSet<String>,
) -> Result<AssetImpact, Error> {
    let storage_files = storage_files(
        &paths.storage,
        &planned.session_ids,
        selection
            .sweep_orphans
            .then_some(&planned.raw_orphans.storage_files),
    )?;
    let snapshot_directories = snapshot_directories(
        &paths.snapshot,
        project_ids,
        selection
            .sweep_orphans
            .then_some(&planned.raw_orphans.snapshot_directories),
    );
    let filesystem_bytes = path_set_bytes(&storage_files, &snapshot_directories)?;
    Ok(AssetImpact {
        storage_files,
        snapshot_directories,
        filesystem_bytes,
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct TableImpact {
    rows: u64,
    bytes: u64,
}

fn candidate_roots<Access>(
    database: &DatabaseConnection<Access>,
    selection: &ImpactSelection,
) -> Result<(SessionIds, Option<ProjectSelectionImpact>), Error> {
    let mut sets = Vec::new();
    if let Some(older) = selection.older_than {
        sets.push(predicates::older_than(database, older.age, older.now_ms)?);
    }
    if selection.archived {
        sets.push(predicates::archived(database)?);
    }
    if let Some(threshold) = selection.larger_than {
        sets.push(subtree::larger_than(database, threshold)?);
    }
    let project_selection = if let Some(project) = &selection.project {
        let project_sessions =
            predicates::project(database, &project.path_or_glob, project.case_sensitivity)?;
        let impact = ProjectSelectionImpact {
            path_or_glob: project.path_or_glob.clone(),
            matched_sessions: to_u64_len(project_sessions.len()),
        };
        sets.push(project_sessions);
        Some(impact)
    } else {
        None
    };
    Ok((
        intersect_candidate_sets(sets).unwrap_or_default(),
        project_selection,
    ))
}

fn selection_roots<Access>(
    database: &DatabaseConnection<Access>,
    candidates: &SessionIds,
) -> Result<SessionIds, Error> {
    let mut statement = database
        .connection()
        .prepare("SELECT id, parent_id FROM session")
        .map_err(|source| sqlite_error("preparing impact root census", source))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(|source| sqlite_error("querying impact roots", source))?;
    let mut roots = SessionIds::new();
    for row in rows {
        let (id, parent_id) = row.map_err(|source| sqlite_error("reading impact root", source))?;
        if candidates.contains(&id)
            && parent_id
                .as_ref()
                .is_none_or(|parent| !candidates.contains(parent))
        {
            roots.insert(id);
        }
    }
    Ok(roots)
}

fn projects_emptied_by<Access>(
    database: &DatabaseConnection<Access>,
    session_ids: &SessionIds,
) -> Result<BTreeSet<String>, Error> {
    let mut statement = database
        .connection()
        .prepare("SELECT id, project_id FROM session")
        .map_err(|source| sqlite_error("preparing project-prune impact census", source))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|source| sqlite_error("querying project-prune impact", source))?;
    let mut selected = BTreeMap::<String, u64>::new();
    let mut total = BTreeMap::<String, u64>::new();
    for row in rows {
        let (session_id, project_id) =
            row.map_err(|source| sqlite_error("reading project-prune impact", source))?;
        *total.entry(project_id.clone()).or_default() += 1;
        if session_ids.contains(&session_id) {
            *selected.entry(project_id).or_default() += 1;
        }
    }
    Ok(selected
        .into_iter()
        .filter_map(|(project_id, selected_count)| {
            (total.get(&project_id) == Some(&selected_count)).then_some(project_id)
        })
        .collect())
}

fn table_impact<Access>(
    database: &DatabaseConnection<Access>,
    specs: &[TableSpec],
    owners: &BTreeSet<String>,
) -> Result<BTreeMap<String, TableImpact>, Error> {
    let mut result = BTreeMap::new();
    if owners.is_empty() {
        return Ok(result);
    }
    for spec in specs {
        let quoted_table = quote_identifier(spec.table);
        let columns = {
            let mut statement = database
                .connection()
                .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
                .map_err(|source| {
                    sqlite_error(&format!("preparing `{}` impact schema", spec.table), source)
                })?;
            statement
                .query_map([spec.table], |row| row.get::<_, String>(0))
                .map_err(|source| {
                    sqlite_error(&format!("querying `{}` impact schema", spec.table), source)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|source| {
                    sqlite_error(&format!("reading `{}` impact schema", spec.table), source)
                })?
        };
        if !columns.iter().any(|column| column == spec.owner_column) {
            return Err(Error::SchemaIncompatible {
                incompatibility: format!(
                    "table `{}` lacks impact owner column `{}`",
                    spec.table, spec.owner_column
                ),
            });
        }
        let byte_expression = columns
            .iter()
            .filter(|column| !spec.attributed_payload_columns.contains(&column.as_str()))
            .map(|column| value_bytes_sql(&quote_identifier(column)))
            .collect::<Vec<_>>()
            .join(" + ");
        let mut impact = TableImpact::default();
        for owner_chunk in owners.iter().collect::<Vec<_>>().chunks(500) {
            let placeholders = std::iter::repeat_n("?", owner_chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT COUNT(*), COALESCE(SUM({byte_expression}), 0) \
                 FROM {quoted_table} WHERE {} IN ({placeholders})",
                quote_identifier(spec.owner_column)
            );
            let (rows, bytes) = database
                .connection()
                .query_row(
                    &sql,
                    rusqlite::params_from_iter(owner_chunk.iter().copied()),
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .map_err(|source| {
                    sqlite_error(&format!("aggregating `{}` impact", spec.table), source)
                })?;
            impact.rows = impact.rows.saturating_add(to_u64(
                rows,
                &format!("reading `{}` impact row count", spec.table),
            )?);
            impact.bytes = impact.bytes.saturating_add(to_u64(
                bytes,
                &format!("reading `{}` impact byte count", spec.table),
            )?);
        }
        result.insert(spec.table.to_owned(), impact);
    }
    Ok(result)
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn value_bytes_sql(column: &str) -> String {
    format!(
        "CASE typeof({column}) WHEN 'null' THEN 0 WHEN 'integer' THEN 8 \
         WHEN 'real' THEN 8 ELSE length(CAST({column} AS BLOB)) END"
    )
}

fn merge_table_impact(
    target: &mut BTreeMap<String, TableImpact>,
    source: BTreeMap<String, TableImpact>,
) {
    for (table, impact) in source {
        let current = target.entry(table).or_default();
        current.rows = current.rows.saturating_add(impact.rows);
        current.bytes = current.bytes.saturating_add(impact.bytes);
    }
}

fn storage_files(
    storage_root: &Path,
    session_ids: &SessionIds,
    raw_orphans: Option<&BTreeSet<PathBuf>>,
) -> Result<BTreeSet<PathBuf>, Error> {
    let mut selected = raw_orphans.cloned().unwrap_or_default();
    for bucket in directory_entries(storage_root)? {
        if !bucket
            .file_type()
            .map_err(|source| io_error(&bucket.path(), source))?
            .is_dir()
        {
            continue;
        }
        for entry in directory_entries(&bucket.path())? {
            if entry
                .file_type()
                .map_err(|source| io_error(&entry.path(), source))?
                .is_file()
                && storage_session_id(&entry.path()).is_some_and(|id| session_ids.contains(id))
            {
                selected.insert(entry.path());
            }
        }
    }
    Ok(selected)
}

fn storage_session_id(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_suffix(".json")?;
    let suffix = id.strip_prefix("ses_")?;
    (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())).then_some(id)
}

fn snapshot_directories(
    snapshot_root: &Path,
    project_ids: &BTreeSet<String>,
    raw_orphans: Option<&BTreeSet<PathBuf>>,
) -> BTreeSet<PathBuf> {
    let mut selected = raw_orphans.cloned().unwrap_or_default();
    selected.extend(
        project_ids
            .iter()
            .map(|project_id| snapshot_root.join(project_id))
            .filter(|path| path.is_dir()),
    );
    selected
}

fn path_set_bytes(
    storage_files: &BTreeSet<PathBuf>,
    snapshot_directories: &BTreeSet<PathBuf>,
) -> Result<u64, Error> {
    let files = storage_files.iter().try_fold(0_u64, |total, path| {
        fs::metadata(path)
            .map(|metadata| total.saturating_add(metadata.len()))
            .map_err(|source| io_error(path, source))
    })?;
    snapshot_directories.iter().try_fold(files, |total, path| {
        directory_bytes(path).map(|bytes| total.saturating_add(bytes))
    })
}

fn directory_bytes(path: &Path) -> Result<u64, Error> {
    let mut bytes = 0_u64;
    for entry in directory_entries(path)? {
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(&entry.path(), source))?;
        if file_type.is_dir() {
            bytes = bytes.saturating_add(directory_bytes(&entry.path())?);
        } else if file_type.is_file() {
            bytes = bytes.saturating_add(
                entry
                    .metadata()
                    .map_err(|source| io_error(&entry.path(), source))?
                    .len(),
            );
        }
    }
    Ok(bytes)
}

fn directory_entries(path: &Path) -> Result<Vec<fs::DirEntry>, Error> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_dir(path)
        .map_err(|source| io_error(path, source))?
        .map(|entry| entry.map_err(|source| io_error(path, source)))
        .collect()
}

#[cfg(test)]
const fn value_bytes(value: ValueRef<'_>) -> u64 {
    match value {
        ValueRef::Null => 0,
        ValueRef::Integer(_) | ValueRef::Real(_) => 8,
        ValueRef::Text(value) | ValueRef::Blob(value) => value.len() as u64,
    }
}

fn to_usize(value: i64, context: &str) -> Result<usize, Error> {
    usize::try_from(value)
        .map_err(|_| sqlite_error(context, rusqlite::Error::IntegralValueOutOfRange(0, value)))
}

fn to_u64(value: i64, context: &str) -> Result<u64, Error> {
    u64::try_from(value)
        .map_err(|_| sqlite_error(context, rusqlite::Error::IntegralValueOutOfRange(0, value)))
}

fn to_u64_len(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        context: context.to_owned(),
        source,
    }
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::hash::{DefaultHasher, Hash, Hasher};

    use rusqlite::params;

    use super::fixture::{Fixture, FixtureConfig};
    use super::*;
    use crate::db::{ConnectionOptions, ReadOnlyConnection, open_read_only};
    use crate::paths::{Target, derived_paths};
    use crate::select::predicates::CaseSensitivity;

    fn open_fixture(fixture: &Fixture) -> ReadOnlyConnection {
        open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only")
    }

    fn file_hash(path: &std::path::Path) -> u64 {
        let mut hasher = DefaultHasher::new();
        fs::read(path)
            .expect("fixture bytes should be readable")
            .hash(&mut hasher);
        hasher.finish()
    }

    fn table_counts(connection: &rusqlite::Connection) -> BTreeMap<String, u64> {
        DELETION_TABLES
            .iter()
            .map(|table| {
                let count = connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .expect("table count should be readable");
                ((*table).to_owned(), u64::try_from(count).unwrap())
            })
            .collect()
    }

    fn legacy_table_impact(
        connection: &rusqlite::Connection,
        specs: &[TableSpec],
        owners: &BTreeSet<String>,
    ) -> BTreeMap<String, TableImpact> {
        specs
            .iter()
            .map(|spec| {
                let mut statement = connection
                    .prepare(&format!("SELECT * FROM {}", spec.table))
                    .expect("legacy impact scan should prepare");
                let owner_index = statement
                    .column_names()
                    .iter()
                    .position(|column| *column == spec.owner_column)
                    .expect("fixture should contain the impact owner column");
                let excluded = statement
                    .column_names()
                    .iter()
                    .enumerate()
                    .filter_map(|(index, column)| {
                        spec.attributed_payload_columns
                            .contains(column)
                            .then_some(index)
                    })
                    .collect::<BTreeSet<_>>();
                let column_count = statement.column_count();
                let mut rows = statement
                    .query([])
                    .expect("legacy impact scan should query");
                let mut impact = TableImpact::default();
                while let Some(row) = rows.next().expect("legacy impact row should be readable") {
                    let owner = row
                        .get::<_, String>(owner_index)
                        .expect("legacy impact owner should be readable");
                    if owners.contains(&owner) {
                        impact.rows = impact.rows.saturating_add(1);
                        for column in 0..column_count {
                            if !excluded.contains(&column) {
                                impact.bytes = impact.bytes.saturating_add(value_bytes(
                                    row.get_ref(column)
                                        .expect("legacy impact value should be readable"),
                                ));
                            }
                        }
                    }
                }
                (spec.table.to_owned(), impact)
            })
            .collect()
    }

    #[test]
    fn attributable_bytes_are_saturating_and_self_contained() {
        assert_eq!(total_attributable_bytes(20, 30, 40), 90);
        assert_eq!(total_attributable_bytes(u64::MAX, 1, 1), u64::MAX);
    }

    #[test]
    fn narrow_selection_matches_legacy_accounting_on_large_relation_tables() {
        let fixture = Fixture::build(&FixtureConfig {
            project_count: 8,
            session_count: 512,
            messages_per_session: 3,
            parts_per_message: 2,
            ..FixtureConfig::default()
        })
        .expect("large relation fixture should build");
        let database = open_fixture(&fixture);
        let selected_session = BTreeSet::from([fixture.session_ids[257].clone()]);
        let selected_project = database
            .connection()
            .query_row(
                "SELECT project_id FROM session WHERE id = ?1",
                [&fixture.session_ids[257]],
                |row| row.get::<_, String>(0),
            )
            .map(|project_id| BTreeSet::from([project_id]))
            .expect("selected project should be readable");

        for (specs, owners) in [
            (SESSION_TABLES, &selected_session),
            (PROJECT_TABLES, &selected_project),
        ] {
            let actual =
                table_impact(&database, specs, owners).expect("impact query should succeed");
            let expected = legacy_table_impact(database.connection(), specs, owners);
            for spec in specs {
                assert_eq!(
                    actual[spec.table].rows, expected[spec.table].rows,
                    "{} rows",
                    spec.table
                );
                assert_eq!(
                    actual[spec.table].bytes, expected[spec.table].bytes,
                    "{} bytes",
                    spec.table
                );
            }
        }
    }

    #[test]
    fn root_count_distinguishes_predicate_roots_from_expanded_descendants() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 2,
            sub_session_depth: 1,
            sub_session_fan_out: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "UPDATE session SET time_archived = 1 WHERE id = 'ses_1'",
                [],
            )
            .expect("root should archive");
        drop(connection);
        let database = open_fixture(&fixture);

        let impact = summarize(
            &database,
            &derived_paths(fixture.root()),
            &ImpactSelection {
                archived: true,
                keep_recent: 1,
                ..ImpactSelection::default()
            },
        )
        .expect("subtree impact should succeed");

        assert_eq!(impact.summary.root_session_count, 1);
        assert_eq!(impact.summary.total_session_count, 2);
    }

    #[test]
    fn larger_than_participates_in_the_predicate_intersection() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 2,
            blob_size_per_part: 600_000,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let database = open_fixture(&fixture);

        let impact = summarize(
            &database,
            &derived_paths(fixture.root()),
            &ImpactSelection {
                archived: true,
                larger_than: Some("1MB".parse().expect("size should parse")),
                ..ImpactSelection::default()
            },
        )
        .expect("size impact should succeed");

        assert!(impact.session_ids.is_empty());
    }

    #[test]
    fn retention_removes_dangling_sessions_from_orphan_sweep() {
        let fixture = Fixture::build(&FixtureConfig {
            dangling_parent_session_count: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "UPDATE session SET time_updated = ?1 WHERE id = 'ses_dangling_0'",
                [super::fixture::BASE_TIME_MS + 1],
            )
            .expect("dangling session timestamp should update");
        drop(connection);
        let database = open_fixture(&fixture);

        let impact = summarize(
            &database,
            &derived_paths(fixture.root()),
            &ImpactSelection {
                keep_recent: 1,
                sweep_orphans: true,
                ..ImpactSelection::default()
            },
        )
        .expect("orphan impact should succeed");

        assert!(!impact.session_ids.contains("ses_dangling_0"));
        assert_eq!(impact.summary.orphan_row_count, 0);
    }

    #[test]
    fn dry_run_summary_leaves_fixture_byte_identical() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 3,
            archived_session_count: 2,
            orphan_event_count: 2,
            orphan_storage_file_count: 1,
            orphan_snapshot_dir_count: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let before = file_hash(&fixture.database_path);
        let database = open_fixture(&fixture);
        let paths = derived_paths(fixture.root());

        let impact = summarize(
            &database,
            &paths,
            &ImpactSelection {
                archived: true,
                sweep_orphans: true,
                ..ImpactSelection::default()
            },
        )
        .expect("impact summary should succeed");

        assert!(impact.summary.total_session_count > 0);
        drop(database);
        assert_eq!(file_hash(&fixture.database_path), before);
    }

    #[test]
    fn project_selection_includes_sessions_with_a_different_working_directory() {
        let fixture = Fixture::build(&FixtureConfig {
            project_count: 2,
            session_count: 4,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "UPDATE session SET directory = '/monorepo/packages/api' WHERE id = 'ses_0'",
                [],
            )
            .expect("session directory should update");
        drop(connection);
        let database = open_fixture(&fixture);

        let impact = summarize(
            &database,
            &derived_paths(fixture.root()),
            &ImpactSelection {
                project: Some(ProjectSelection {
                    path_or_glob: "/fixture/project-0".to_owned(),
                    case_sensitivity: CaseSensitivity::Sensitive,
                }),
                ..ImpactSelection::default()
            },
        )
        .expect("project impact should succeed");

        assert!(impact.session_ids.contains("ses_0"));
        assert_eq!(
            impact
                .summary
                .project_selection
                .as_ref()
                .unwrap()
                .matched_sessions,
            2
        );
        let mut rendered = Vec::new();
        write_human(&impact.summary, &mut rendered).expect("impact should render");
        assert!(
            String::from_utf8(rendered)
                .unwrap()
                .contains("selects ALL 2 member sessions regardless of each session directory")
        );
        assert_eq!(impact.summary.project_prune_count, 1);
        assert_eq!(impact.summary.table_rows["project"], 1);
        assert_eq!(impact.summary.table_rows["project_directory"], 1);
        assert_eq!(impact.summary.table_rows["permission"], 1);
        assert_eq!(impact.summary.table_rows["workspace"], 1);
        assert_eq!(impact.summary.storage_file_count, 2);
        assert_eq!(impact.summary.snapshot_directory_count, 1);
    }

    #[test]
    fn predicted_row_deletions_match_raw_sql_apply_proxy() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 2,
            archived_session_count: 1,
            messages_per_session: 2,
            parts_per_message: 2,
            orphan_event_count: 2,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "INSERT INTO event_sequence VALUES ('ses_OrphanApply', 1, NULL)",
                [],
            )
            .expect("valid orphan aggregate should insert");
        connection
            .execute(
                "INSERT INTO event VALUES ('event-valid-orphan', 'ses_OrphanApply', 1, 'session.orphan', '{}')",
                [],
            )
            .expect("valid orphan event should insert");
        drop(connection);
        let database = open_fixture(&fixture);
        let impact = summarize(
            &database,
            &derived_paths(fixture.root()),
            &ImpactSelection {
                archived: true,
                sweep_orphans: true,
                ..ImpactSelection::default()
            },
        )
        .expect("impact summary should succeed");
        assert!(
            impact
                .orphan_event_aggregate_ids
                .contains("ses_OrphanApply")
        );
        drop(database);

        let connection = fixture.connect().expect("fixture should connect");
        let before = table_counts(&connection);
        for session_id in &impact.session_ids {
            connection
                .execute("DELETE FROM session WHERE id = ?1", params![session_id])
                .expect("session proxy deletion should succeed");
            connection
                .execute(
                    "DELETE FROM event_sequence WHERE aggregate_id = ?1",
                    params![session_id],
                )
                .expect("session event proxy deletion should succeed");
        }
        for aggregate_id in &impact.orphan_event_aggregate_ids {
            connection
                .execute(
                    "DELETE FROM event_sequence WHERE aggregate_id = ?1",
                    params![aggregate_id],
                )
                .expect("orphan event proxy deletion should succeed");
        }
        let after = table_counts(&connection);

        for (table, predicted) in &impact.summary.table_rows {
            assert_eq!(before[table] - after[table], *predicted, "{table}");
        }
        for aggregate_id in &impact.orphan_event_aggregate_ids {
            assert_eq!(
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM event_sequence WHERE aggregate_id = ?1",
                        [aggregate_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("remaining selected orphan events should be countable"),
                0_i64
            );
        }
    }
}
