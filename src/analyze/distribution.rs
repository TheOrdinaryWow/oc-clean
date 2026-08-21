use std::fs;
use std::path::Path;

use rusqlite::params;

use crate::db::{DatabaseConnection, sqlite_error};
use crate::error::Error;
use crate::paths::DerivedPaths;

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

const DAY_MS: i64 = 86_400_000;

const AGE_DISTRIBUTION_SQL: &str = "
    WITH payload AS (
        SELECT session_id, SUM(bytes) AS bytes
        FROM (
            SELECT session_id, octet_length(data) AS bytes FROM message
            UNION ALL
            SELECT session_id, octet_length(data) AS bytes FROM part
            UNION ALL
            SELECT session_id, COALESCE(SUM(octet_length(baseline) + octet_length(snapshot)), 0) AS bytes
            FROM session_context_epoch AS source
            GROUP BY session_id
            UNION ALL
            SELECT session_id, octet_length(data) AS bytes FROM session_message
            UNION ALL
            SELECT aggregate_id AS session_id, octet_length(data) AS bytes FROM event
        )
        GROUP BY session_id
    ), bucketed AS (
        SELECT CASE
                   WHEN session.time_updated > ?1 THEN 0
                   WHEN session.time_updated > ?2 THEN 1
                   WHEN session.time_updated > ?3 THEN 2
                   WHEN session.time_updated > ?4 THEN 3
                   ELSE 4
               END AS bucket,
               COALESCE(payload.bytes, 0) AS bytes
        FROM session
        LEFT JOIN payload ON payload.session_id = session.id
    )
    SELECT bucket, COUNT(*), COALESCE(SUM(bytes), 0)
    FROM bucketed
    GROUP BY bucket
    ORDER BY bucket";

/// A fixed session-age interval used by the distribution report.
///
/// Intervals are left-closed and right-open by age: `0-7D` contains ages below seven days, while
/// sessions exactly 7, 30, 90, or 180 days old enter the bucket beginning at that boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgeRange {
    Days0To7,
    Days7To30,
    Days30To90,
    Days90To180,
    Days180Plus,
}

/// Session count and payload bytes for one age interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgeBucket {
    pub range: AgeRange,
    pub sessions: u64,
    pub bytes: u64,
}

/// Recursive file count and byte total for a directory.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DirectorySummary {
    pub files: u64,
    pub bytes: u64,
}

/// One named immediate child and its recursive file accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedDirectorySummary {
    pub name: String,
    pub files: u64,
    pub bytes: u64,
}

/// A directory total plus deterministic accounting for its immediate child directories.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DirectoryGroup {
    pub total: DirectorySummary,
    pub entries: Vec<NamedDirectorySummary>,
}

/// Read-only accounting for external `OpenCode` directories.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExternalDirectoryOverview {
    pub storage: DirectoryGroup,
    pub snapshot: DirectoryGroup,
    pub tool_output: DirectorySummary,
    pub log: DirectorySummary,
}

/// Session age distribution and external-directory accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DistributionReport {
    pub age_buckets: Vec<AgeBucket>,
    pub external: ExternalDirectoryOverview,
}

/// Analyzes session age and external directory usage at an injected millisecond epoch.
///
/// # Errors
///
/// Returns [`Error::DatabaseBusy`] for SQLite lock contention, [`Error::Sqlite`] for other age
/// query failures, or [`Error::Io`] when an existing external directory cannot be read.
pub fn analyze<Access>(
    database: &DatabaseConnection<Access>,
    paths: &DerivedPaths,
    now_ms: i64,
) -> Result<DistributionReport, Error> {
    Ok(DistributionReport {
        age_buckets: age_distribution(database, now_ms)?,
        external: external_directories(paths)?,
    })
}

fn age_distribution<Access>(
    database: &DatabaseConnection<Access>,
    now_ms: i64,
) -> Result<Vec<AgeBucket>, Error> {
    let boundaries = [7, 30, 90, 180]
        .map(|days| boundary_ms(now_ms, days))
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let mut statement = database
        .connection()
        .prepare(AGE_DISTRIBUTION_SQL)
        .map_err(|source| sqlite_error("preparing session age distribution", source))?;
    let rows = statement
        .query_map(
            params![boundaries[0], boundaries[1], boundaries[2], boundaries[3]],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|source| sqlite_error("querying session age distribution", source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error("reading session age distribution", source))?;
    buckets_from_rows(&rows)
}

fn boundary_ms(now_ms: i64, days: i64) -> Result<i64, Error> {
    now_ms
        .checked_sub(days * DAY_MS)
        .ok_or_else(|| Error::InvalidArgument {
            argument: "now_ms".to_owned(),
            reason: "cannot represent age bucket boundaries as millisecond epochs".to_owned(),
        })
}

fn buckets_from_rows(rows: &[(i64, i64, i64)]) -> Result<Vec<AgeBucket>, Error> {
    let ranges = [
        AgeRange::Days0To7,
        AgeRange::Days7To30,
        AgeRange::Days30To90,
        AgeRange::Days90To180,
        AgeRange::Days180Plus,
    ];
    let mut buckets = ranges.map(|range| AgeBucket {
        range,
        sessions: 0,
        bytes: 0,
    });
    for &(bucket, sessions, bytes) in rows {
        let index = usize::try_from(bucket)
            .ok()
            .filter(|index| *index < buckets.len())
            .ok_or_else(|| integral_error(0, bucket, "reading session age bucket"))?;
        buckets[index].sessions = to_u64(1, sessions, "reading session count")?;
        buckets[index].bytes = to_u64(2, bytes, "reading session payload bytes")?;
    }
    Ok(buckets.into())
}

fn external_directories(paths: &DerivedPaths) -> Result<ExternalDirectoryOverview, Error> {
    Ok(ExternalDirectoryOverview {
        storage: scan_group(&paths.storage)?,
        snapshot: scan_group(&paths.snapshot)?,
        tool_output: scan_directory(&paths.tool_output)?,
        log: scan_directory(&paths.log)?,
    })
}

fn scan_group(root: &Path) -> Result<DirectoryGroup, Error> {
    let Some(entries) = read_directory(root)? else {
        return Ok(DirectoryGroup::default());
    };
    let mut group = DirectoryGroup::default();
    for entry in entries {
        let entry = entry.map_err(|source| io_error(root, source))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(&path, source))?;
        if file_type.is_dir() {
            let summary = scan_directory(&path)?;
            group.entries.push(NamedDirectorySummary {
                name: entry.file_name().to_string_lossy().into_owned(),
                files: summary.files,
                bytes: summary.bytes,
            });
            add_summary(&mut group.total, summary);
        } else {
            add_file(&mut group.total, &path)?;
        }
    }
    group
        .entries
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(group)
}

fn scan_directory(root: &Path) -> Result<DirectorySummary, Error> {
    let Some(entries) = read_directory(root)? else {
        return Ok(DirectorySummary::default());
    };
    let mut summary = DirectorySummary::default();
    for entry in entries {
        let entry = entry.map_err(|source| io_error(root, source))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(&path, source))?;
        if file_type.is_dir() {
            add_summary(&mut summary, scan_directory(&path)?);
        } else {
            add_file(&mut summary, &path)?;
        }
    }
    Ok(summary)
}

fn read_directory(root: &Path) -> Result<Option<fs::ReadDir>, Error> {
    match fs::read_dir(root) {
        Ok(entries) => Ok(Some(entries)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) if is_malformed_path(&source) => Ok(None),
        Err(source) => Err(io_error(root, source)),
    }
}

/// Reports whether the platform rejected the path as unusable rather than merely absent.
///
/// Windows returns `ERROR_INVALID_NAME` for a syntactically impossible path, which is how the
/// placeholder siblings of an in-memory target appear there. Unix reports the same situation as
/// a plain missing directory.
#[cfg(windows)]
fn is_malformed_path(source: &std::io::Error) -> bool {
    const ERROR_INVALID_NAME: i32 = 123;

    source.raw_os_error() == Some(ERROR_INVALID_NAME)
}

#[cfg(not(windows))]
const fn is_malformed_path(_source: &std::io::Error) -> bool {
    false
}

fn add_file(summary: &mut DirectorySummary, path: &Path) -> Result<(), Error> {
    let bytes = fs::symlink_metadata(path)
        .map_err(|source| io_error(path, source))?
        .len();
    summary.files = summary.files.saturating_add(1);
    summary.bytes = summary.bytes.saturating_add(bytes);
    Ok(())
}

fn add_summary(total: &mut DirectorySummary, addition: DirectorySummary) {
    total.files = total.files.saturating_add(addition.files);
    total.bytes = total.bytes.saturating_add(addition.bytes);
}

fn to_u64(column: usize, value: i64, context: &str) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| integral_error(column, value, context))
}

fn integral_error(column: usize, value: i64, context: &str) -> Error {
    sqlite_error(
        context,
        rusqlite::Error::IntegralValueOutOfRange(column, value),
    )
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::params;

    use super::*;
    use crate::db::{ConnectionOptions, open_read_only};
    use crate::paths::{Target, derived_paths};

    use super::fixture::{Fixture, FixtureConfig};

    const KNOWN_NOW_MS: i64 = fixture::BASE_TIME_MS;

    fn open_fixture(fixture: &Fixture) -> crate::db::ReadOnlyConnection {
        open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only")
    }

    fn set_session_ages(fixture: &Fixture, ages_days: &[i64]) {
        let connection = fixture.connect().expect("fixture should connect");
        for (session_id, age_days) in fixture.session_ids.iter().zip(ages_days) {
            connection
                .execute(
                    "UPDATE session SET time_updated = ?1 WHERE id = ?2",
                    params![KNOWN_NOW_MS - age_days * DAY_MS, session_id],
                )
                .expect("session age should update");
        }
    }

    #[test]
    fn known_ages_report_counts_bytes_and_left_closed_boundaries() {
        let config = FixtureConfig {
            session_count: 10,
            blob_size_per_part: 5,
            ..FixtureConfig::default()
        };
        let fixture = Fixture::build(&config).expect("fixture should build");
        set_session_ages(&fixture, &[0, 6, 7, 29, 30, 89, 90, 179, 180, 365]);
        let database = open_fixture(&fixture);

        let report = analyze(&database, &derived_paths(fixture.root()), KNOWN_NOW_MS)
            .expect("distribution should succeed");

        assert_eq!(
            report.age_buckets,
            vec![
                AgeBucket {
                    range: AgeRange::Days0To7,
                    sessions: 2,
                    bytes: 52,
                },
                AgeBucket {
                    range: AgeRange::Days7To30,
                    sessions: 2,
                    bytes: 52,
                },
                AgeBucket {
                    range: AgeRange::Days30To90,
                    sessions: 2,
                    bytes: 52,
                },
                AgeBucket {
                    range: AgeRange::Days90To180,
                    sessions: 2,
                    bytes: 52,
                },
                AgeBucket {
                    range: AgeRange::Days180Plus,
                    sessions: 2,
                    bytes: 52,
                },
            ]
        );
    }

    #[test]
    fn epoch_only_payload_matches_session_attribution() {
        let config = FixtureConfig {
            project_count: 1,
            session_count: 1,
            messages_per_session: 0,
            parts_per_message: 0,
            ..FixtureConfig::default()
        };
        let fixture = Fixture::build(&config).expect("fixture should build");
        let session_id = &fixture.session_ids[0];
        let baseline = "epoch baseline";
        let snapshot = "epoch snapshot";
        let expected_bytes = u64::try_from(baseline.len() + snapshot.len())
            .expect("payload length should fit in u64");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute("DELETE FROM event WHERE aggregate_id = ?1", [session_id])
            .expect("event payload should be removed");
        connection
            .execute(
                "UPDATE session_context_epoch SET baseline = ?1, snapshot = ?2 WHERE session_id = ?3",
                params![baseline, snapshot, session_id],
            )
            .expect("context epoch payload should update");
        drop(connection);
        let database = open_fixture(&fixture);

        let distribution = analyze(&database, &derived_paths(fixture.root()), KNOWN_NOW_MS)
            .expect("distribution should succeed");
        let attribution =
            crate::analyze::attribution::analyze(&database, 1).expect("attribution should succeed");

        assert_eq!(distribution.age_buckets[0].bytes, expected_bytes);
        assert_eq!(attribution.sessions[0].session_id, *session_id);
        assert_eq!(attribution.sessions[0].self_bytes, expected_bytes);
        assert_eq!(
            distribution.age_buckets[0].bytes,
            attribution.sessions[0].self_bytes
        );
    }

    #[test]
    fn age_distribution_sql_uses_plain_integer_comparisons() {
        let normalized = AGE_DISTRIBUTION_SQL.to_ascii_lowercase();

        assert!(!normalized.contains("julianday"));
        assert!(!normalized.contains("datetime("));
        assert!(normalized.contains("time_updated"));
    }

    #[test]
    fn generated_external_tree_reports_storage_snapshot_tool_output_and_log() {
        let config = FixtureConfig {
            project_count: 2,
            session_count: 3,
            orphan_storage_file_count: 1,
            orphan_snapshot_dir_count: 1,
            ..FixtureConfig::default()
        };
        let fixture = Fixture::build(&config).expect("fixture should build");
        let paths = derived_paths(fixture.root());
        fs::create_dir_all(paths.tool_output.join("nested")).expect("tool-output should create");
        fs::write(paths.tool_output.join("result.txt"), b"tool").expect("tool file should write");
        fs::write(paths.tool_output.join("nested/more.txt"), b"output")
            .expect("nested tool file should write");
        fs::create_dir_all(&paths.log).expect("log should create");
        fs::write(paths.log.join("current.log"), b"logging").expect("log file should write");
        let database = open_fixture(&fixture);

        let report = analyze(&database, &paths, KNOWN_NOW_MS).expect("distribution should succeed");

        assert_eq!(
            report.external.storage,
            DirectoryGroup {
                total: DirectorySummary { files: 4, bytes: 8 },
                entries: vec![NamedDirectorySummary {
                    name: "session".to_owned(),
                    files: 4,
                    bytes: 8,
                }],
            }
        );
        assert_eq!(report.external.snapshot.total.files, 3);
        assert_eq!(report.external.snapshot.total.bytes, 63);
        assert_eq!(
            report
                .external
                .snapshot
                .entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.files, entry.bytes))
                .collect::<Vec<_>>(),
            vec![
                ("project-0", 1, 21),
                ("project-1", 1, 21),
                ("project-orphan-0", 1, 21),
            ]
        );
        assert_eq!(
            report.external.tool_output,
            DirectorySummary {
                files: 2,
                bytes: 10,
            }
        );
        assert_eq!(report.external.log, DirectorySummary { files: 1, bytes: 7 });
    }

    #[test]
    fn absent_snapshot_directory_reports_zero() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        fs::remove_dir_all(&fixture.snapshot_dir).expect("snapshot fixture should be removable");
        let database = open_fixture(&fixture);

        let report = analyze(&database, &derived_paths(fixture.root()), KNOWN_NOW_MS)
            .expect("missing snapshot should degrade to zero");

        assert_eq!(report.external.snapshot, DirectoryGroup::default());
    }
}
