use std::collections::BTreeMap;

use crate::db::{self, DatabaseConnection};
use crate::error::Error;
use crate::select::predicates::{SessionIds, effective_time_updated};
use crate::select::subtree;

const RETENTION_ROOTS_SQL: &str = r"
SELECT root.id, root.project_id
FROM session AS root
WHERE root.parent_id IS NULL
    OR NOT EXISTS (
        SELECT 1
        FROM session AS parent
        WHERE parent.id = root.parent_id
    )
ORDER BY root.project_id, root.id
";

#[derive(Debug, Eq, PartialEq)]
struct RetentionRoot {
    session_id: String,
    project_id: String,
    effective_time_updated: i64,
}

/// Computes the hard-retention set for the most recently active roots in each project.
///
/// # Errors
///
/// Returns [`Error::SchemaIncompatible`] when `session.parent_id` contains a cycle, or
/// [`Error::Sqlite`] when retention queries fail.
pub fn compute<Access>(
    database: &DatabaseConnection<Access>,
    keep_recent: u64,
) -> Result<SessionIds, Error> {
    let effective_times = effective_time_updated(database)?
        .into_iter()
        .map(|session| (session.session_id, session.time_updated))
        .collect::<BTreeMap<_, _>>();
    if keep_recent == 0 {
        return Ok(SessionIds::new());
    }

    let mut statement = database
        .connection()
        .prepare(RETENTION_ROOTS_SQL)
        .map_err(|source| sqlite_error("preparing retention-root query", source))?;
    let mut roots = statement
        .query_map([], |row| {
            let session_id = row.get::<_, String>(0)?;
            let project_id = row.get::<_, String>(1)?;
            Ok((session_id, project_id))
        })
        .map_err(|source| sqlite_error("querying retention roots", source))?
        .map(|row| {
            let (session_id, project_id) =
                row.map_err(|source| sqlite_error("reading retention root", source))?;
            let effective_time_updated =
                effective_times.get(&session_id).copied().ok_or_else(|| {
                    Error::SchemaIncompatible {
                        incompatibility: format!(
                            "effective timestamp missing for retention root {session_id}"
                        ),
                    }
                })?;
            Ok(RetentionRoot {
                session_id,
                project_id,
                effective_time_updated,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;

    roots.sort_by(|left, right| {
        left.project_id
            .cmp(&right.project_id)
            .then_with(|| {
                right
                    .effective_time_updated
                    .cmp(&left.effective_time_updated)
            })
            .then_with(|| right.session_id.cmp(&left.session_id))
    });

    let mut project_counts = BTreeMap::<String, u64>::new();
    let selected_roots = roots
        .into_iter()
        .filter_map(|root| {
            let count = project_counts.entry(root.project_id).or_default();
            if *count >= keep_recent {
                return None;
            }
            *count += 1;
            Some(root.session_id)
        })
        .collect();

    subtree::expand(database, &selected_roots)
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    db::sqlite_error(context, source)
}

#[cfg(test)]
#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::{Duration as StdDuration, Instant};

    use rusqlite::params;

    use super::fixture::{BASE_TIME_MS, Fixture, FixtureConfig};
    use super::*;
    use crate::cli::types::Duration;
    use crate::db::{ConnectionOptions, ReadOnlyConnection, open_read_only};
    use crate::paths::Target;
    use crate::select::predicates::older_than;

    const DAY_MS: i64 = 86_400_000;

    fn open_fixture(fixture: &Fixture) -> ReadOnlyConnection {
        open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only")
    }

    fn ids(values: &[&str]) -> SessionIds {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn set_updated(fixture: &Fixture, values: &[(&str, i64)]) {
        let connection = fixture.connect().expect("fixture should connect");
        for (session_id, time_updated) in values {
            connection
                .execute(
                    "UPDATE session SET time_updated = ?1 WHERE id = ?2",
                    params![time_updated, session_id],
                )
                .expect("session timestamp should update");
        }
    }

    #[test]
    fn keeps_three_recent_roots_and_their_complete_subtrees() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 5,
            sub_session_depth: 1,
            sub_session_fan_out: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        set_updated(
            &fixture,
            &[
                ("ses_0", BASE_TIME_MS - 10 * DAY_MS),
                ("ses_0_d0_c0", BASE_TIME_MS - 10 * DAY_MS),
                ("ses_1", BASE_TIME_MS - 9 * DAY_MS),
                ("ses_1_d0_c0", BASE_TIME_MS - DAY_MS),
                ("ses_2", BASE_TIME_MS - 8 * DAY_MS),
                ("ses_2_d0_c0", BASE_TIME_MS - 8 * DAY_MS),
                ("ses_3", BASE_TIME_MS - 7 * DAY_MS),
                ("ses_3_d0_c0", BASE_TIME_MS - 7 * DAY_MS),
                ("ses_4", BASE_TIME_MS - 6 * DAY_MS),
                ("ses_4_d0_c0", BASE_TIME_MS - 6 * DAY_MS),
            ],
        );

        let database = open_fixture(&fixture);
        let candidates = older_than(&database, Duration::from_str("1D").unwrap(), BASE_TIME_MS)
            .expect("age selection should succeed");
        let retained = compute(&database, 3).unwrap();

        assert_eq!(candidates.len(), 10);
        assert_eq!(candidates.difference(&retained).count(), 4);
        assert_eq!(
            retained,
            ids(&[
                "ses_1",
                "ses_1_d0_c0",
                "ses_3",
                "ses_3_d0_c0",
                "ses_4",
                "ses_4_d0_c0",
            ])
        );
    }

    #[test]
    fn applies_the_quota_independently_per_project() {
        let fixture = Fixture::build(&FixtureConfig {
            project_count: 2,
            session_count: 8,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");

        let retained = compute(&open_fixture(&fixture), 3).unwrap();

        assert_eq!(
            retained,
            ids(&["ses_0", "ses_1", "ses_2", "ses_3", "ses_4", "ses_5"])
        );
    }

    #[test]
    fn retains_every_root_when_the_project_has_fewer_than_the_quota() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 2,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");

        assert_eq!(
            compute(&open_fixture(&fixture), 5).unwrap(),
            ids(&["ses_0", "ses_1"])
        );
    }

    #[test]
    fn breaks_equal_timestamp_ties_by_descending_identifier() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 4,
            time_span_ms: 0,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");

        assert_eq!(
            compute(&open_fixture(&fixture), 2).unwrap(),
            ids(&["ses_2", "ses_3"])
        );
    }

    #[test]
    fn dangling_root_and_its_subtree_are_protected_by_retention_only() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 2,
            dangling_parent_session_count: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "UPDATE session SET parent_id = 'ses_dangling_0' WHERE id = 'ses_1'",
                [],
            )
            .expect("child should attach to dangling root");
        drop(connection);
        set_updated(
            &fixture,
            &[
                ("ses_0", BASE_TIME_MS - 2 * DAY_MS),
                ("ses_1", BASE_TIME_MS - DAY_MS),
                ("ses_dangling_0", BASE_TIME_MS),
            ],
        );
        let database = open_fixture(&fixture);

        assert_eq!(
            compute(&database, 1).unwrap(),
            ids(&["ses_dangling_0", "ses_1"])
        );
        assert!(compute(&database, 0).unwrap().is_empty());
    }

    #[test]
    fn reports_a_parent_cycle_within_a_bound() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 2,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "UPDATE session SET parent_id = 'ses_1' WHERE id = 'ses_0'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE session SET parent_id = 'ses_0' WHERE id = 'ses_1'",
                [],
            )
            .unwrap();
        drop(connection);
        let started = Instant::now();

        let error = compute(&open_fixture(&fixture), 1).unwrap_err();

        assert!(started.elapsed() < StdDuration::from_secs(1));
        assert!(matches!(error, Error::SchemaIncompatible { .. }));
        assert!(error.to_string().contains("session.parent_id cycle"));
    }
}
