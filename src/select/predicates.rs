use std::collections::BTreeSet;

use crate::cli::types::Duration;
use crate::db::DatabaseConnection;
use crate::error::Error;

const EFFECTIVE_TIME_UPDATED_SQL: &str = r"
WITH RECURSIVE subtree(root_id, session_id, time_updated, path, cycle) AS (
    SELECT id, id, time_updated, char(31) || id || char(31), 0
    FROM session
    UNION ALL
    SELECT
        subtree.root_id,
        child.id,
        child.time_updated,
        subtree.path || child.id || char(31),
        instr(subtree.path, char(31) || child.id || char(31)) > 0
    FROM subtree
    JOIN session AS child ON child.parent_id = subtree.session_id
    WHERE subtree.cycle = 0
),
effective_age(root_id, effective_time_updated, cycle) AS MATERIALIZED (
    SELECT root_id, MAX(time_updated), MAX(cycle)
    FROM subtree
    GROUP BY root_id
)
SELECT root_id, effective_time_updated, cycle
FROM effective_age
ORDER BY root_id
";

const ARCHIVED_SQL: &str = "SELECT id FROM session WHERE time_archived IS NOT NULL ORDER BY id";

const PROJECT_PATHS_SQL: &str = r"
SELECT session.id, project.worktree, project_directory.directory
FROM session
JOIN project ON project.id = session.project_id
LEFT JOIN project_directory ON project_directory.project_id = project.id
ORDER BY session.id, project_directory.directory
";

/// A deterministic set of candidate session identifiers.
pub type SessionIds = BTreeSet<String>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveTimeUpdated {
    pub session_id: String,
    pub time_updated: i64,
}

/// Filesystem case policy used for project-path matching.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaseSensitivity {
    Sensitive,
    Insensitive,
}

/// Selects sessions whose complete descendant subtree has been inactive for at least `age`.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when the boundary cannot be represented as an `i64`
/// millisecond epoch, [`Error::SchemaIncompatible`] when `session.parent_id` contains a cycle, or
/// [`Error::Sqlite`] when SQLite cannot execute or decode the query.
pub fn older_than<Access>(
    database: &DatabaseConnection<Access>,
    age: Duration,
    now_ms: i64,
) -> Result<SessionIds, Error> {
    let age_ms = i64::try_from(age.as_millis()).map_err(|_| Error::InvalidArgument {
        argument: "older-than".to_owned(),
        reason: "duration exceeds the supported millisecond epoch range".to_owned(),
    })?;
    let boundary_ms = now_ms
        .checked_sub(age_ms)
        .ok_or_else(|| Error::InvalidArgument {
            argument: "older-than".to_owned(),
            reason: "age boundary is outside the supported millisecond epoch range".to_owned(),
        })?;
    Ok(effective_time_updated(database)?
        .into_iter()
        .filter(|session| session.time_updated <= boundary_ms)
        .map(|session| session.session_id)
        .collect())
}

pub(crate) fn effective_time_updated<Access>(
    database: &DatabaseConnection<Access>,
) -> Result<Vec<EffectiveTimeUpdated>, Error> {
    let mut statement = database
        .connection()
        .prepare(EFFECTIVE_TIME_UPDATED_SQL)
        .map_err(|source| sqlite_error("preparing effective session age query", source))?;
    let mut rows = statement
        .query([])
        .map_err(|source| sqlite_error("querying effective session ages", source))?;
    let mut effective_times = Vec::new();

    while let Some(row) = rows
        .next()
        .map_err(|source| sqlite_error("reading effective session ages", source))?
    {
        let session_id = row
            .get::<_, String>(0)
            .map_err(|source| sqlite_error("reading effective-age session id", source))?;
        let time_updated = row
            .get::<_, i64>(1)
            .map_err(|source| sqlite_error("reading effective session timestamp", source))?;
        let cycle = row
            .get::<_, i64>(2)
            .map_err(|source| sqlite_error("reading effective-age cycle marker", source))?;
        if cycle != 0 {
            return Err(parent_cycle(&session_id));
        }
        effective_times.push(EffectiveTimeUpdated {
            session_id,
            time_updated,
        });
    }

    Ok(effective_times)
}

/// Selects sessions carrying a non-null archive epoch.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when SQLite cannot execute or decode the query.
pub fn archived<Access>(database: &DatabaseConnection<Access>) -> Result<SessionIds, Error> {
    query_session_ids(database, ARCHIVED_SQL, "querying archived sessions")
}

/// Selects sessions belonging to a project matched by worktree or registered project directory.
///
/// Glob syntax is activated by `*`, `?`, or `[`. Literal paths have trailing separators removed.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] for malformed character classes or [`Error::Sqlite`] when
/// SQLite cannot execute or decode the query.
pub fn project<Access>(
    database: &DatabaseConnection<Access>,
    path_or_glob: &str,
    case_sensitivity: CaseSensitivity,
) -> Result<SessionIds, Error> {
    let matcher = ProjectPathMatcher::new(path_or_glob, case_sensitivity)?;
    let mut statement = database
        .connection()
        .prepare(PROJECT_PATHS_SQL)
        .map_err(|source| sqlite_error("preparing project session query", source))?;
    let mut rows = statement
        .query([])
        .map_err(|source| sqlite_error("querying project sessions", source))?;
    let mut selected = SessionIds::new();

    while let Some(row) = rows
        .next()
        .map_err(|source| sqlite_error("reading project sessions", source))?
    {
        let session_id = row
            .get::<_, String>(0)
            .map_err(|source| sqlite_error("reading project session id", source))?;
        let worktree = row
            .get::<_, String>(1)
            .map_err(|source| sqlite_error("reading project worktree", source))?;
        let directory = row
            .get::<_, Option<String>>(2)
            .map_err(|source| sqlite_error("reading registered project directory", source))?;
        if matcher.matches(&worktree)
            || directory
                .as_deref()
                .is_some_and(|path| matcher.matches(path))
        {
            selected.insert(session_id);
        }
    }

    Ok(selected)
}

/// Applies project path normalization, case policy, and optional glob matching to one stored path.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when the glob contains an unterminated character class.
pub fn matches_project_path(
    path_or_glob: &str,
    stored_path: &str,
    case_sensitivity: CaseSensitivity,
) -> Result<bool, Error> {
    Ok(ProjectPathMatcher::new(path_or_glob, case_sensitivity)?.matches(stored_path))
}

/// Intersects every supplied predicate result.
///
/// `None` represents the identity state where no session predicate has been supplied yet.
pub fn intersect_candidate_sets<I>(sets: I) -> Option<SessionIds>
where
    I: IntoIterator<Item = SessionIds>,
{
    let mut sets = sets.into_iter();
    let mut intersection = sets.next()?;
    for candidates in sets {
        intersection.retain(|session_id| candidates.contains(session_id));
    }
    Some(intersection)
}

fn query_session_ids<Access>(
    database: &DatabaseConnection<Access>,
    sql: &str,
    context: &str,
) -> Result<SessionIds, Error> {
    let mut statement = database
        .connection()
        .prepare(sql)
        .map_err(|source| sqlite_error(context, source))?;
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| sqlite_error(context, source))?
        .collect::<Result<SessionIds, _>>()
        .map_err(|source| sqlite_error(context, source))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProjectPathMatcher {
    Exact {
        path: String,
        case_sensitivity: CaseSensitivity,
    },
    Glob {
        tokens: Vec<GlobToken>,
        case_sensitivity: CaseSensitivity,
    },
}

impl ProjectPathMatcher {
    fn new(input: &str, case_sensitivity: CaseSensitivity) -> Result<Self, Error> {
        if contains_glob_metacharacter(input) {
            Ok(Self::Glob {
                tokens: parse_glob(&fold_case(input, case_sensitivity))?,
                case_sensitivity,
            })
        } else {
            Ok(Self::Exact {
                path: fold_case(normalize_literal_path(input), case_sensitivity),
                case_sensitivity,
            })
        }
    }

    fn matches(&self, stored_path: &str) -> bool {
        let normalized = normalize_literal_path(stored_path);
        match self {
            Self::Exact {
                path,
                case_sensitivity,
            } => *path == fold_case(normalized, *case_sensitivity),
            Self::Glob {
                tokens,
                case_sensitivity,
            } => glob_matches(tokens, &fold_case(normalized, *case_sensitivity)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GlobToken {
    AnySequence,
    AnyCharacter,
    CharacterClass {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
    Literal(char),
}

fn contains_glob_metacharacter(value: &str) -> bool {
    value.contains(['*', '?', '['])
}

fn normalize_literal_path(path: &str) -> &str {
    let normalized = path.trim_end_matches(['/', '\\']);
    if normalized.is_empty() && !path.is_empty() {
        &path[..1]
    } else {
        normalized
    }
}

fn fold_case(value: &str, case_sensitivity: CaseSensitivity) -> String {
    match case_sensitivity {
        CaseSensitivity::Sensitive => value.to_owned(),
        CaseSensitivity::Insensitive => value.to_lowercase(),
    }
}

fn parse_glob(pattern: &str) -> Result<Vec<GlobToken>, Error> {
    let characters = pattern.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '*' => {
                if !matches!(tokens.last(), Some(GlobToken::AnySequence)) {
                    tokens.push(GlobToken::AnySequence);
                }
                index += 1;
            }
            '?' => {
                tokens.push(GlobToken::AnyCharacter);
                index += 1;
            }
            '[' => {
                let (token, next_index) = parse_character_class(&characters, index)?;
                tokens.push(token);
                index = next_index;
            }
            literal => {
                tokens.push(GlobToken::Literal(literal));
                index += 1;
            }
        }
    }
    Ok(tokens)
}

fn parse_character_class(
    pattern: &[char],
    opening_index: usize,
) -> Result<(GlobToken, usize), Error> {
    let closing_offset = pattern[opening_index + 1..]
        .iter()
        .position(|character| *character == ']')
        .ok_or_else(|| Error::InvalidArgument {
            argument: "project".to_owned(),
            reason: "glob contains an unterminated character class".to_owned(),
        })?;
    let closing_index = opening_index + closing_offset + 1;
    let mut content = &pattern[opening_index + 1..closing_index];
    let negated = content
        .first()
        .is_some_and(|character| matches!(character, '!' | '^'));
    if negated {
        content = &content[1..];
    }
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < content.len() {
        if index + 2 < content.len() && content[index + 1] == '-' {
            ranges.push((content[index], content[index + 2]));
            index += 3;
        } else {
            ranges.push((content[index], content[index]));
            index += 1;
        }
    }
    Ok((
        GlobToken::CharacterClass { negated, ranges },
        closing_index + 1,
    ))
}

fn glob_matches(tokens: &[GlobToken], value: &str) -> bool {
    let mut states = vec![false; tokens.len() + 1];
    states[0] = true;
    expand_stars(tokens, &mut states);

    for character in value.chars() {
        let mut next = vec![false; tokens.len() + 1];
        for (index, token) in tokens.iter().enumerate() {
            if !states[index] {
                continue;
            }
            match token {
                GlobToken::AnySequence => next[index] = true,
                GlobToken::AnyCharacter => next[index + 1] = true,
                GlobToken::CharacterClass { negated, ranges } => {
                    let contained = ranges
                        .iter()
                        .any(|(start, end)| *start <= character && character <= *end);
                    if contained != *negated {
                        next[index + 1] = true;
                    }
                }
                GlobToken::Literal(literal) if *literal == character => next[index + 1] = true,
                GlobToken::Literal(_) => {}
            }
        }
        expand_stars(tokens, &mut next);
        states = next;
    }

    expand_stars(tokens, &mut states);
    states[tokens.len()]
}

fn expand_stars(tokens: &[GlobToken], states: &mut [bool]) {
    for (index, token) in tokens.iter().enumerate() {
        if states[index] && matches!(token, GlobToken::AnySequence) {
            states[index + 1] = true;
        }
    }
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
    use std::str::FromStr;
    use std::time::{Duration as StdDuration, Instant};

    use rusqlite::params;

    use super::fixture::{Fixture, FixtureConfig};
    use super::*;
    use crate::db::{ConnectionOptions, ReadOnlyConnection, open_read_only};
    use crate::paths::Target;

    const DAY_MS: i64 = 86_400_000;
    const NOW_MS: i64 = fixture::BASE_TIME_MS;

    fn open_fixture(fixture: &Fixture) -> ReadOnlyConnection {
        open_read_only(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions::default(),
        )
        .expect("fixture should open read-only")
    }

    fn ten_sessions() -> Fixture {
        Fixture::build(&FixtureConfig {
            session_count: 10,
            ..FixtureConfig::default()
        })
        .expect("fixture should build")
    }

    fn set_ages(fixture: &Fixture, ages_days: &[i64]) {
        let connection = fixture.connect().expect("fixture should connect");
        for (session_id, age_days) in fixture.session_ids.iter().zip(ages_days) {
            connection
                .execute(
                    "UPDATE session SET time_updated = ?1 WHERE id = ?2",
                    params![NOW_MS - age_days * DAY_MS, session_id],
                )
                .expect("session age should update");
        }
    }

    fn ids(values: &[&str]) -> SessionIds {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn older_than_selects_known_effective_ages_at_the_integer_boundary() {
        let fixture = ten_sessions();
        set_ages(&fixture, &[0, 1, 7, 29, 30, 31, 60, 89, 90, 365]);
        let database = open_fixture(&fixture);
        let cases = [
            (
                "30D",
                ids(&["ses_4", "ses_5", "ses_6", "ses_7", "ses_8", "ses_9"]),
            ),
            ("90D", ids(&["ses_8", "ses_9"])),
            ("1Y", ids(&["ses_9"])),
        ];

        for (input, expected) in cases {
            let age = Duration::from_str(input).expect("duration should parse");
            assert_eq!(older_than(&database, age, NOW_MS).unwrap(), expected);
        }
    }

    #[test]
    fn effective_age_uses_the_newest_timestamp_in_the_whole_subtree() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 1,
            sub_session_depth: 1,
            sub_session_fan_out: 1,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        set_ages(&fixture, &[60, 1]);
        let database = open_fixture(&fixture);

        let selected = older_than(&database, Duration::from_str("30D").unwrap(), NOW_MS).unwrap();

        assert!(selected.is_empty());
    }

    #[test]
    fn older_than_detects_a_two_node_parent_cycle_within_a_bound() {
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
        let database = open_fixture(&fixture);
        let started = Instant::now();

        let error = older_than(&database, Duration::from_str("30D").unwrap(), NOW_MS).unwrap_err();

        assert!(started.elapsed() < StdDuration::from_secs(1));
        assert!(matches!(error, Error::SchemaIncompatible { .. }));
        assert!(error.to_string().contains("session.parent_id cycle"));
    }

    #[test]
    fn archived_selects_only_sessions_with_an_archive_epoch() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 10,
            archived_session_count: 3,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");

        assert_eq!(
            archived(&open_fixture(&fixture)).unwrap(),
            ids(&["ses_0", "ses_1", "ses_2"])
        );
    }

    #[test]
    fn project_matches_worktree_and_project_directory_without_session_directory() {
        let fixture = Fixture::build(&FixtureConfig {
            project_count: 2,
            session_count: 4,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let connection = fixture.connect().unwrap();
        connection
            .execute(
                "INSERT INTO project_directory (project_id,directory,type,strategy,time_created) VALUES ('project-1','/aliases/mono','worktree','git',?1)",
                [NOW_MS],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE session SET directory = '/aliases/mono' WHERE id = 'ses_0'",
                [],
            )
            .unwrap();
        drop(connection);
        let database = open_fixture(&fixture);

        assert_eq!(
            project(&database, "/fixture/project-0", CaseSensitivity::Sensitive,).unwrap(),
            ids(&["ses_0", "ses_2"])
        );
        assert_eq!(
            project(&database, "/aliases/mono", CaseSensitivity::Sensitive).unwrap(),
            ids(&["ses_1", "ses_3"])
        );
    }

    #[test]
    fn project_path_matching_is_table_driven_for_separators_globs_and_case() {
        let cases = [
            (
                "/repo/main/",
                "/repo/main",
                CaseSensitivity::Sensitive,
                true,
            ),
            (
                "/repo/main",
                "/repo/main///",
                CaseSensitivity::Sensitive,
                true,
            ),
            (
                "/repo/project-[ab]",
                "/repo/project-a",
                CaseSensitivity::Sensitive,
                true,
            ),
            (
                "/repo/project-?",
                "/repo/project-c",
                CaseSensitivity::Sensitive,
                true,
            ),
            (
                "/repo/*/src",
                "/repo/app/src",
                CaseSensitivity::Sensitive,
                true,
            ),
            (
                "/Repo/Main",
                "/repo/main",
                CaseSensitivity::Sensitive,
                false,
            ),
            (
                "/Repo/Main",
                "/repo/main",
                CaseSensitivity::Insensitive,
                true,
            ),
        ];

        for (filter, stored, sensitivity, expected) in cases {
            assert_eq!(
                matches_project_path(filter, stored, sensitivity).unwrap(),
                expected,
                "filter={filter}, stored={stored}"
            );
        }
    }

    #[test]
    fn project_glob_selects_the_expected_project_subset() {
        let fixture = Fixture::build(&FixtureConfig {
            project_count: 3,
            session_count: 6,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");

        assert_eq!(
            project(
                &open_fixture(&fixture),
                "/fixture/project-[02]",
                CaseSensitivity::Sensitive,
            )
            .unwrap(),
            ids(&["ses_0", "ses_2", "ses_3", "ses_5"])
        );
    }

    #[test]
    fn intersection_composition_only_narrows_candidate_sets() {
        let age = ids(&["ses_1", "ses_2", "ses_3"]);
        let project = ids(&["ses_2", "ses_3", "ses_4"]);
        let archived = ids(&["ses_3", "ses_5"]);

        let selected = intersect_candidate_sets([age.clone(), project, archived]).unwrap();

        assert_eq!(selected, ids(&["ses_3"]));
        assert!(selected.is_subset(&age));
        assert!(intersect_candidate_sets(Vec::new()).is_none());
    }
}
