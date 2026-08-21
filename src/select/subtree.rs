use rusqlite::{Connection, OptionalExtension, params_from_iter};

use crate::analyze::attribution;
use crate::cli::types::Size;
use crate::db::DatabaseConnection;
use crate::error::Error;

use super::predicates::SessionIds;

/// Expands selected sessions to include every transitive descendant.
///
/// # Errors
///
/// Returns [`Error::SchemaIncompatible`] when `session.parent_id` contains a cycle or
/// [`Error::Sqlite`] when SQLite cannot execute or decode the query.
pub fn expand<Access>(
    database: &DatabaseConnection<Access>,
    selected: &SessionIds,
) -> Result<SessionIds, Error> {
    if selected.is_empty() {
        return Ok(SessionIds::new());
    }

    let mut expanded = SessionIds::new();

    for selected_chunk in selected.iter().collect::<Vec<_>>().chunks(500) {
        let placeholders = std::iter::repeat_n("?", selected_chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            r"
            WITH RECURSIVE subtree(root_id, session_id, path, cycle) AS (
                SELECT id, id, char(31) || id || char(31), 0
                FROM session
                WHERE id IN ({placeholders})
                UNION ALL
                SELECT
                    subtree.root_id,
                    child.id,
                    subtree.path || child.id || char(31),
                    instr(subtree.path, char(31) || child.id || char(31)) > 0
                FROM subtree
                JOIN session AS child ON child.parent_id = subtree.session_id
                WHERE subtree.cycle = 0
            )
            SELECT root_id, session_id, cycle
            FROM subtree
            ORDER BY root_id, session_id
            "
        );
        let mut statement = database.connection().prepare(&sql).map_err(|source| {
            sqlite_error("preparing selected session subtree expansion", source)
        })?;
        let mut rows = statement
            .query(params_from_iter(selected_chunk.iter().copied()))
            .map_err(|source| {
                sqlite_error("querying selected session subtree expansion", source)
            })?;

        while let Some(row) = rows
            .next()
            .map_err(|source| sqlite_error("reading selected session subtree expansion", source))?
        {
            let root_id = row
                .get::<_, String>(0)
                .map_err(|source| sqlite_error("reading subtree expansion root id", source))?;
            let session_id = row
                .get::<_, String>(1)
                .map_err(|source| sqlite_error("reading expanded session id", source))?;
            let cycle = row
                .get::<_, i64>(2)
                .map_err(|source| sqlite_error("reading subtree expansion cycle marker", source))?;
            if cycle != 0 {
                return Err(parent_cycle(&root_id));
            }
            expanded.insert(session_id);
        }
    }

    Ok(expanded)
}

/// Selects sessions whose self-plus-subtree payload size meets `threshold`.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when `octet_length()` is unavailable,
/// [`Error::SchemaIncompatible`] when parent links cycle or part attribution diverges, or
/// [`Error::Sqlite`] when SQLite cannot execute or decode the query.
pub fn larger_than<Access>(
    database: &DatabaseConnection<Access>,
    threshold: Size,
) -> Result<SessionIds, Error> {
    validate_part_attribution(database.connection())?;
    let session_count = database
        .connection()
        .query_row("SELECT COUNT(*) FROM session", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|source| sqlite_error("counting sessions for size selection", source))?;
    let top_n = usize::try_from(session_count).map_err(|_| {
        sqlite_error(
            "reading session count for size selection",
            rusqlite::Error::IntegralValueOutOfRange(0, session_count),
        )
    })?;
    let report = attribution::analyze(database, top_n)?;

    Ok(report
        .sessions
        .into_iter()
        .filter(|session| session.subtree_bytes >= threshold.as_bytes())
        .map(|session| session.session_id)
        .collect())
}

fn validate_part_attribution(connection: &Connection) -> Result<(), Error> {
    let divergence = connection
        .query_row(
            r"
            SELECT part.id, part.session_id, message.session_id
            FROM part
            LEFT JOIN message ON message.id = part.message_id
            WHERE message.id IS NULL OR part.session_id IS NOT message.session_id
            ORDER BY part.id
            LIMIT 1
            ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|source| sqlite_error("validating part session attribution", source))?;
    let Some((part_id, direct_session_id, message_session_id)) = divergence else {
        return Ok(());
    };
    let message_session_id = message_session_id.as_deref().unwrap_or("<missing message>");
    Err(Error::SchemaIncompatible {
        incompatibility: format!(
            "part.session_id attribution diverges from the message_id join path for part `{part_id}`: direct `{direct_session_id}`, message `{message_session_id}`"
        ),
    })
}

fn parent_cycle(root_id: &str) -> Error {
    Error::SchemaIncompatible {
        incompatibility: format!(
            "session.parent_id cycle detected while traversing descendants from `{root_id}`"
        ),
    }
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        context: context.to_owned(),
        source,
    }
}

#[cfg(test)]
#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::str::FromStr;
    use std::time::{Duration as StdDuration, Instant};

    use super::fixture::{self, Fixture, FixtureConfig};
    use super::*;
    use crate::cli::types::Duration;
    use crate::db::{ConnectionOptions, ReadOnlyConnection, open_read_only};
    use crate::paths::Target;
    use crate::select::predicates::{intersect_candidate_sets, older_than};

    const DAY_MS: i64 = 86_400_000;
    const NOW_MS: i64 = fixture::BASE_TIME_MS;

    fn open_fixture(fixture: &Fixture) -> ReadOnlyConnection {
        open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only")
    }

    fn ids(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn tree_fixture() -> Fixture {
        Fixture::build(&FixtureConfig {
            session_count: 1,
            messages_per_session: 1,
            parts_per_message: 1,
            blob_size_per_part: 0,
            sub_session_depth: 2,
            sub_session_fan_out: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build")
    }

    fn clear_payloads(fixture: &Fixture) {
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute_batch(
                "UPDATE message SET data = '';
                 UPDATE part SET data = '';
                 UPDATE session_message SET data = '';
                 UPDATE event SET data = '';
                 UPDATE session_context_epoch SET baseline = '', snapshot = '';",
            )
            .expect("payloads should clear");
    }

    #[test]
    fn expands_a_root_through_three_session_levels() {
        let fixture = tree_fixture();
        let database = open_fixture(&fixture);

        let expanded = expand(&database, &ids(&["ses_0"])).unwrap();

        assert_eq!(
            expanded,
            ids(&["ses_0", "ses_0_d0_c0", "ses_0_d0_c0_d1_c0"])
        );
    }

    #[test]
    fn expansion_supports_more_ids_than_sqlites_default_variable_limit() {
        let fixture = tree_fixture();
        let database = open_fixture(&fixture);
        let mut selected = (0..33_000)
            .map(|index| format!("missing_{index:05}"))
            .collect::<SessionIds>();
        selected.insert("ses_0".to_owned());

        let expanded = expand(&database, &selected).unwrap();

        assert_eq!(
            expanded,
            ids(&["ses_0", "ses_0_d0_c0", "ses_0_d0_c0_d1_c0"])
        );
    }

    #[test]
    fn expansion_of_an_empty_set_is_empty() {
        let fixture = tree_fixture();

        assert!(
            expand(&open_fixture(&fixture), &SessionIds::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn expansion_reports_a_parent_cycle_within_a_bound() {
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

        let error = expand(&open_fixture(&fixture), &ids(&["ses_0"])).unwrap_err();

        assert!(started.elapsed() < StdDuration::from_secs(1));
        assert!(matches!(error, Error::SchemaIncompatible { .. }));
        assert!(error.to_string().contains("session.parent_id cycle"));
    }

    #[test]
    fn larger_than_uses_the_complete_descendant_subtree_total() {
        let fixture = tree_fixture();
        clear_payloads(&fixture);
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "UPDATE part SET data = zeroblob(80) WHERE session_id = 'ses_0_d0_c0'",
                [],
            )
            .unwrap();
        drop(connection);

        let selected = larger_than(
            &open_fixture(&fixture),
            Size::from_str("0.000080MB").expect("valid size"),
        )
        .unwrap();

        assert_eq!(selected, ids(&["ses_0", "ses_0_d0_c0"]));
    }

    #[test]
    fn part_attribution_agrees_with_the_message_join_path_and_reports_divergence() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 2,
            messages_per_session: 1,
            parts_per_message: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let database = open_fixture(&fixture);
        assert!(larger_than(&database, Size::from_str("0MB").unwrap()).is_ok());
        drop(database);
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "UPDATE part SET session_id = 'ses_1' WHERE message_id = 'msg-ses_0-0'",
                [],
            )
            .unwrap();
        drop(connection);

        let error =
            larger_than(&open_fixture(&fixture), Size::from_str("0MB").unwrap()).unwrap_err();

        assert!(matches!(error, Error::SchemaIncompatible { .. }));
        assert!(error.to_string().contains("part.session_id"));
        assert!(error.to_string().contains("part-msg-ses_0-0-0"));
    }

    #[test]
    fn older_than_and_larger_than_intersection_respects_subtree_effective_age() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 1,
            sub_session_depth: 1,
            sub_session_fan_out: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        connection
            .execute(
                "UPDATE session SET time_updated = ?1 WHERE id = 'ses_0'",
                [NOW_MS - 60 * DAY_MS],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE session SET time_updated = ?1 WHERE id = 'ses_0_d0_c0'",
                [NOW_MS - DAY_MS],
            )
            .unwrap();
        drop(connection);
        let database = open_fixture(&fixture);
        let age = older_than(&database, Duration::from_str("30D").unwrap(), NOW_MS).unwrap();
        let size = larger_than(&database, Size::from_str("0MB").unwrap()).unwrap();

        let selected = intersect_candidate_sets([age, size]).expect("two predicates supplied");

        assert!(!selected.contains("ses_0"));
    }
}
