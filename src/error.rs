use std::path::PathBuf;

use thiserror::Error as ThisError;

/// Failures reported by `oc-clean`.
///
/// This enum is non-exhaustive so downstream callers must retain a fallback arm when matching it.
/// That keeps adding a newly categorized failure source-compatible.
///
/// The exit-code contract is stable:
///
/// - `0`: complete success (never returned by an `Error`)
/// - `1`: generic I/O or SQLite failure
/// - `2`: invalid argument, or an operation the operator canceled
/// - `3`: database not found
/// - `4`: incompatible schema
/// - `5`: database busy
/// - `6`: insufficient disk space
/// - `7`: integrity check failed
/// - `8`: interrupted
/// - `9`: unsupported platform
/// - `10`: partial success
/// - `11`: fatal swap rollback failure
///
/// Downstream exhaustive matches are rejected, preserving extension safety:
///
/// ```compile_fail
/// use oc_clean::error::Error;
///
/// fn classify(error: &Error) -> i32 {
///     match error {
///         Error::SchemaIncompatible { .. } => 4,
///         Error::DatabaseBusy { .. } => 5,
///         Error::InsufficientDiskSpace { .. } => 6,
///         Error::IntegrityCheckFailed { .. } => 7,
///         Error::Interrupted { .. } => 8,
///         Error::PartialSuccess { .. } => 10,
///         Error::SwapRollbackFailed { .. } => 11,
///         Error::InvalidArgument { .. } | Error::Canceled { .. } => 2,
///         Error::Io { .. } | Error::Sqlite { .. } => 1,
///         Error::NotFound { .. } => 3,
///         Error::UnsupportedPlatform { .. } => 9,
///     }
/// }
/// ```
#[derive(Debug, ThisError)]
#[non_exhaustive]
pub enum Error {
    #[error("database schema is incompatible: {incompatibility}")]
    SchemaIncompatible { incompatibility: String },

    #[error("database is busy; observed holders: {holders:?}")]
    DatabaseBusy { holders: Vec<String> },

    #[error(
        "insufficient disk space: {required_bytes} bytes required, {available_bytes} bytes available"
    )]
    InsufficientDiskSpace {
        required_bytes: u64,
        available_bytes: u64,
    },

    #[error("database reclaim strategy is unavailable: {reason}")]
    ReclaimUnavailable { reason: String },

    #[error("SQLite {check} failed: {message}")]
    IntegrityCheckFailed { check: String, message: String },

    #[error("operation interrupted by SIGINT after {completed}")]
    Interrupted { completed: String },

    #[error("operation completed, but cleanup is incomplete; left behind: {left_behind}")]
    PartialSuccess { left_behind: String },

    #[error(
        "FATAL: database swap and rollback both failed for `{database_path}` and backup `{backup_path}`; manual recovery is required",
        database_path = database_path.display(),
        backup_path = backup_path.display()
    )]
    SwapRollbackFailed {
        database_path: PathBuf,
        backup_path: PathBuf,
    },

    #[error("invalid argument `{argument}`: {reason}")]
    InvalidArgument { argument: String, reason: String },

    /// The operator declined a destructive command, or never answered its confirmation.
    ///
    /// This shares exit code 2 with [`Self::InvalidArgument`] so a script can still branch on a
    /// non-zero status, but it is a decision rather than a fault and is rendered without the
    /// `error:` prefix.
    #[error("{reason}")]
    Canceled { reason: String },

    #[error("I/O operation failed for `{path}`: {source}", path = path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("SQLite operation failed while {context}: {source}")]
    Sqlite {
        context: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error("database file does not exist: `{path}`", path = path.display())]
    NotFound { path: PathBuf },

    #[error("platform `{platform}` is unsupported")]
    UnsupportedPlatform { platform: String },
}

impl Error {
    /// Returns the stable process exit code for this failure category.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Io { .. } | Self::Sqlite { .. } => 1,
            Self::InvalidArgument { .. } | Self::Canceled { .. } => 2,
            Self::NotFound { .. } => 3,
            Self::SchemaIncompatible { .. } => 4,
            Self::DatabaseBusy { .. } => 5,
            Self::InsufficientDiskSpace { .. } | Self::ReclaimUnavailable { .. } => 6,
            Self::IntegrityCheckFailed { .. } => 7,
            Self::Interrupted { .. } => 8,
            Self::UnsupportedPlatform { .. } => 9,
            Self::PartialSuccess { .. } => 10,
            Self::SwapRollbackFailed { .. } => 11,
        }
    }

    /// Returns the stable machine-readable identifier for this failure category.
    ///
    /// The identifier is part of the JSON error contract and changes only alongside
    /// the exit-code table, so automation can branch on it without parsing prose.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Io { .. } => "io",
            Self::Sqlite { .. } => "sqlite",
            Self::InvalidArgument { .. } => "invalid_argument",
            Self::Canceled { .. } => "canceled",
            Self::NotFound { .. } => "not_found",
            Self::SchemaIncompatible { .. } => "schema_incompatible",
            Self::DatabaseBusy { .. } => "database_busy",
            Self::InsufficientDiskSpace { .. } => "insufficient_disk_space",
            Self::ReclaimUnavailable { .. } => "reclaim_unavailable",
            Self::IntegrityCheckFailed { .. } => "integrity_check_failed",
            Self::Interrupted { .. } => "interrupted",
            Self::UnsupportedPlatform { .. } => "unsupported_platform",
            Self::PartialSuccess { .. } => "partial_success",
            Self::SwapRollbackFailed { .. } => "swap_rollback_failed",
        }
    }

    /// Returns the next action an operator can take, when one is well defined.
    #[must_use]
    pub const fn hint(&self) -> Option<&'static str> {
        match self {
            Self::DatabaseBusy { .. } => {
                Some("stop OpenCode, then retry; `--force` downgrades the holder gate to a warning")
            }
            Self::SchemaIncompatible { .. } => {
                Some("`--force-schema` downgrades Tier 3 findings; Tier 1 always stops the command")
            }
            Self::InsufficientDiskSpace { .. } => {
                Some("free disk space, or use `--no-vacuum` to delete rows without a rebuild")
            }
            Self::ReclaimUnavailable { .. } => {
                Some("a full rebuild reclaims space when incremental auto-vacuum is unavailable")
            }
            Self::Interrupted { .. } => Some("completed batches are committed; rerun to continue"),
            Self::PartialSuccess { .. } => Some("remove the listed leftovers manually"),
            Self::SwapRollbackFailed { .. } => {
                Some("restore the named backup by hand before running any further command")
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io,
        path::{Path, PathBuf},
    };

    use super::Error;

    type ErrorCase = (&'static str, Error, i32, &'static str);

    fn variants() -> Vec<ErrorCase> {
        let mut variants = infrastructure_variants();
        variants.extend(operation_variants());
        variants
    }

    fn infrastructure_variants() -> Vec<ErrorCase> {
        vec![
            (
                "io",
                Error::Io {
                    path: PathBuf::from("/tmp/opencode.db"),
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "access denied"),
                },
                1,
                "/tmp/opencode.db",
            ),
            (
                "sqlite",
                Error::Sqlite {
                    context: "reading session rows".to_owned(),
                    source: rusqlite::Error::InvalidQuery,
                },
                1,
                "reading session rows",
            ),
            (
                "invalid_argument",
                Error::InvalidArgument {
                    argument: "--older-than".to_owned(),
                    reason: "expected a whole number of days".to_owned(),
                },
                2,
                "--older-than",
            ),
            (
                "not_found",
                Error::NotFound {
                    path: PathBuf::from("/tmp/missing.db"),
                },
                3,
                "/tmp/missing.db",
            ),
            (
                "schema_incompatible",
                Error::SchemaIncompatible {
                    incompatibility: "required session.time_updated column is missing".to_owned(),
                },
                4,
                "session.time_updated",
            ),
            (
                "database_busy",
                Error::DatabaseBusy {
                    holders: vec!["opencode (pid 42)".to_owned()],
                },
                5,
                "opencode (pid 42)",
            ),
        ]
    }

    fn operation_variants() -> Vec<ErrorCase> {
        vec![
            (
                "insufficient_disk_space",
                Error::InsufficientDiskSpace {
                    required_bytes: 2_000,
                    available_bytes: 1_000,
                },
                6,
                "2000",
            ),
            (
                "reclaim_unavailable",
                Error::ReclaimUnavailable {
                    reason: "incremental auto-vacuum is disabled".to_owned(),
                },
                6,
                "incremental auto-vacuum",
            ),
            (
                "integrity_check_failed",
                Error::IntegrityCheckFailed {
                    check: "foreign_key_check".to_owned(),
                    message: "child row references a missing parent".to_owned(),
                },
                7,
                "foreign_key_check",
            ),
            (
                "interrupted",
                Error::Interrupted {
                    completed: "deleted 3 of 8 batches".to_owned(),
                },
                8,
                "deleted 3 of 8 batches",
            ),
            (
                "unsupported_platform",
                Error::UnsupportedPlatform {
                    platform: "dragonfly".to_owned(),
                },
                9,
                "dragonfly",
            ),
            (
                "partial_success",
                Error::PartialSuccess {
                    left_behind: "snapshot /tmp/snapshot/proj_1".to_owned(),
                },
                10,
                "/tmp/snapshot/proj_1",
            ),
            (
                "swap_rollback_failed",
                Error::SwapRollbackFailed {
                    database_path: PathBuf::from("/tmp/opencode.db"),
                    backup_path: PathBuf::from("/tmp/opencode.db.bak.20260821T000000Z"),
                },
                11,
                "manual recovery",
            ),
        ]
    }

    #[test]
    fn error_exit_codes_match_stable_contract() {
        for (name, error, expected_code, _) in variants() {
            assert_eq!(error.exit_code(), expected_code, "variant {name}");
        }
    }

    #[test]
    fn error_messages_are_actionable() {
        for (name, error, _, expected_fragment) in variants() {
            let message = error.to_string();
            assert!(!message.trim().is_empty(), "variant {name}");
            assert!(
                message.contains(expected_fragment),
                "variant {name} rendered `{message}` without `{expected_fragment}`"
            );
        }
    }

    #[test]
    fn failure_categories_have_unique_exit_codes() {
        let mut names_by_code = BTreeMap::<i32, Vec<&str>>::new();
        for (name, error, _, _) in variants() {
            names_by_code
                .entry(error.exit_code())
                .or_default()
                .push(name);
        }

        assert_eq!(names_by_code.len(), 11);
        assert_eq!(names_by_code.get(&1), Some(&vec!["io", "sqlite"]));
        assert!(
            names_by_code
                .iter()
                .all(|(&code, names)| code == 1 || code == 6 || names.len() == 1)
        );
    }

    #[test]
    fn swap_rollback_paths_are_absolute() {
        let root = if cfg!(windows) {
            PathBuf::from("C:\\opencode")
        } else {
            PathBuf::from("/var/lib/opencode")
        };
        let error = Error::SwapRollbackFailed {
            database_path: root.join("opencode.db"),
            backup_path: root.join("opencode.db.bak"),
        };

        let Error::SwapRollbackFailed {
            database_path,
            backup_path,
        } = error
        else {
            unreachable!("constructed the swap rollback variant")
        };
        assert!(Path::new(&database_path).is_absolute());
        assert!(Path::new(&backup_path).is_absolute());
    }
}
