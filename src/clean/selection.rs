use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::CleanArgs;
use crate::db::{DatabaseConnection, ReadWrite};
use crate::error::Error;
use crate::report::impact::{ImpactSelection, OlderThanSelection, ProjectSelection};
use crate::select::predicates::{self, CaseSensitivity, SessionIds};
use crate::select::{retention, subtree};

pub(super) fn retention_set(
    database: &DatabaseConnection<ReadWrite>,
    keep_recent: u64,
) -> Result<SessionIds, Error> {
    retention::compute(database, keep_recent)
}

pub(super) fn predicate_candidates(
    database: &DatabaseConnection<ReadWrite>,
    arguments: &CleanArgs,
    retained: &SessionIds,
    now_ms: i64,
) -> Result<SessionIds, Error> {
    let mut sets = Vec::new();
    if let Some(age) = arguments.older_than {
        sets.push(predicates::older_than(database, age, now_ms)?);
    }
    if let Some(project) = &arguments.project {
        sets.push(predicates::project(database, project, case_sensitivity())?);
    }
    if let Some(size) = arguments.larger_than {
        sets.push(subtree::larger_than(database, size)?);
    }
    if arguments.archived {
        sets.push(predicates::archived(database)?);
    }
    for set in &mut sets {
        set.retain(|session_id| !retained.contains(session_id));
    }
    Ok(predicates::intersect_candidate_sets(sets).unwrap_or_default())
}

pub(super) fn expand_candidates(
    database: &DatabaseConnection<ReadWrite>,
    candidates: &SessionIds,
    retained: &SessionIds,
) -> Result<SessionIds, Error> {
    let mut expanded = subtree::expand(database, candidates)?;
    expanded.retain(|session_id| !retained.contains(session_id));
    Ok(expanded)
}

pub(super) fn impact_selection(arguments: &CleanArgs, now_ms: i64) -> ImpactSelection {
    ImpactSelection {
        older_than: arguments
            .older_than
            .map(|age| OlderThanSelection { age, now_ms }),
        archived: arguments.archived,
        project: arguments
            .project
            .as_ref()
            .map(|path_or_glob| ProjectSelection {
                path_or_glob: path_or_glob.clone(),
                case_sensitivity: case_sensitivity(),
            }),
        larger_than: arguments.larger_than,
        keep_recent: arguments.keep_recent,
        sweep_orphans: arguments.orphans,
        preview_top: arguments.top,
    }
}

pub(super) fn now_ms() -> Result<i64, Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| Error::InvalidArgument {
            argument: "system clock".to_owned(),
            reason: source.to_string(),
        })?;
    i64::try_from(elapsed.as_millis()).map_err(|_| Error::InvalidArgument {
        argument: "system clock".to_owned(),
        reason: "current timestamp exceeds SQLite integer range".to_owned(),
    })
}

const fn case_sensitivity() -> CaseSensitivity {
    platform_case_sensitivity(cfg!(windows), cfg!(target_os = "macos"))
}

const fn platform_case_sensitivity(is_windows: bool, is_macos: bool) -> CaseSensitivity {
    if is_windows || is_macos {
        CaseSensitivity::Insensitive
    } else {
        CaseSensitivity::Sensitive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_case_sensitivity_is_host_independent() {
        assert_eq!(
            platform_case_sensitivity(false, true),
            CaseSensitivity::Insensitive
        );
        assert_eq!(
            platform_case_sensitivity(true, false),
            CaseSensitivity::Insensitive
        );
        assert_eq!(
            platform_case_sensitivity(false, false),
            CaseSensitivity::Sensitive
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn native_case_insensitive_platform_uses_insensitive_matching() {
        assert_eq!(case_sensitivity(), CaseSensitivity::Insensitive);
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn native_case_sensitive_platform_uses_sensitive_matching() {
        assert_eq!(case_sensitivity(), CaseSensitivity::Sensitive);
    }
}
