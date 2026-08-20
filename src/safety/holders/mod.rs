//! Cross-platform process-holder inspection and command gating.

use std::path::{Path, PathBuf};

use crate::error::Error;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// A process with one or more open database-related files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HolderInfo {
    /// Operating-system process identifier.
    pub pid: u32,
    /// Best-effort process name.
    pub process_name: Option<String>,
    /// Database, WAL, or SHM paths held by this process.
    pub matched_paths: Vec<PathBuf>,
}

/// Whether a scan covered all processes visible to the current user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completeness {
    /// Every visible process was inspected.
    CompleteForVisibleProcesses,
    /// Permission restrictions prevented inspection of at least one process.
    PartialDueToPermissions,
    /// The platform or process API could not perform a meaningful scan.
    Unsupported,
}

/// Three-state result of inspecting database holders.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// At least one process holds the database, WAL, or SHM file.
    Held(Vec<HolderInfo>),
    /// The complete visible-process scan found no holders.
    NotHeld,
    /// The scan could not establish whether any holder exists.
    CannotDetermine(String),
}

/// Holder verdict paired with scan coverage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inspection {
    /// Holder result.
    pub verdict: Verdict,
    /// Coverage achieved by the inspector.
    pub completeness: Completeness,
}

/// Platform implementation capable of finding open database files.
pub trait HolderInspector {
    /// Inspects the main database path and its `-wal` and `-shm` siblings.
    fn inspect(&self, database_path: &Path) -> Inspection;
}

/// Command mode whose holder policy must be evaluated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandMode {
    /// Read-only analysis.
    Analyze,
    /// Read-only diagnostics.
    Doctor,
    /// Cleanup preview or application.
    Clean { apply: bool },
    /// Vacuum preview or application.
    Vacuum { apply: bool },
}

/// Action selected by the holder gating matrix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GateDecision {
    /// Continue without a holder warning.
    Proceed,
    /// Continue after reporting the holder state.
    Warn,
    /// Refuse a destructive operation because holders were observed.
    RefuseHeld(Vec<HolderInfo>),
    /// Refuse a destructive operation because holder status is unknown.
    RefuseCannotDetermine(String),
}

impl GateDecision {
    /// Converts a destructive refusal into the stable database-busy error category.
    ///
    /// Warning and proceed decisions remain successful so read-only commands and forced writes can
    /// continue after the caller reports the inspection details.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DatabaseBusy`] for `RefuseHeld` and `RefuseCannotDetermine`.
    pub fn into_result(self) -> Result<(), Error> {
        match self {
            Self::Proceed | Self::Warn => Ok(()),
            Self::RefuseHeld(holders) => Err(Error::DatabaseBusy {
                holders: holders.iter().map(holder_description).collect(),
            }),
            Self::RefuseCannotDetermine(reason) => Err(Error::DatabaseBusy {
                holders: vec![format!("cannot determine process holders: {reason}")],
            }),
        }
    }
}

fn holder_description(holder: &HolderInfo) -> String {
    let name = holder.process_name.as_deref().unwrap_or("unknown process");
    let paths = holder
        .matched_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("pid {} ({name}) holds {paths}", holder.pid)
}

/// Runs an inspector and applies the command's holder policy.
#[must_use]
pub fn inspect_and_decide(
    inspector: &dyn HolderInspector,
    database_path: &Path,
    command: CommandMode,
    force: bool,
) -> (Inspection, GateDecision) {
    let inspection = inspector.inspect(database_path);
    let destructive = matches!(
        command,
        CommandMode::Clean { apply: true } | CommandMode::Vacuum { apply: true }
    );
    let decision = match &inspection.verdict {
        Verdict::NotHeld => GateDecision::Proceed,
        Verdict::Held(holders) if destructive && !force => {
            GateDecision::RefuseHeld(holders.clone())
        }
        Verdict::CannotDetermine(reason) if destructive && !force => {
            GateDecision::RefuseCannotDetermine(reason.clone())
        }
        Verdict::Held(_) | Verdict::CannotDetermine(_) => GateDecision::Warn,
    };
    (inspection, decision)
}

pub(crate) fn database_related_paths(database_path: &Path) -> [PathBuf; 3] {
    let database_path =
        std::fs::canonicalize(database_path).unwrap_or_else(|_| absolute_path(database_path));
    [
        database_path.clone(),
        path_with_suffix(&database_path, "-wal"),
        path_with_suffix(&database_path, "-shm"),
    ]
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_owned(), |current| current.join(path))
    }
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut suffixed = path.as_os_str().to_owned();
    suffixed.push(suffix);
    PathBuf::from(suffixed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FakeInspector(Inspection);

    impl HolderInspector for FakeInspector {
        fn inspect(&self, _database_path: &Path) -> Inspection {
            self.0.clone()
        }
    }

    fn held() -> Inspection {
        Inspection {
            verdict: Verdict::Held(vec![HolderInfo {
                pid: 42,
                process_name: Some("opencode".to_owned()),
                matched_paths: vec![PathBuf::from("opencode.db")],
            }]),
            completeness: Completeness::CompleteForVisibleProcesses,
        }
    }

    fn not_held() -> Inspection {
        Inspection {
            verdict: Verdict::NotHeld,
            completeness: Completeness::CompleteForVisibleProcesses,
        }
    }

    fn unknown() -> Inspection {
        Inspection {
            verdict: Verdict::CannotDetermine("permission denied".to_owned()),
            completeness: Completeness::PartialDueToPermissions,
        }
    }

    #[test]
    fn gating_matrix_is_table_driven() {
        let states = [held(), not_held(), unknown()];
        let commands = [
            CommandMode::Analyze,
            CommandMode::Doctor,
            CommandMode::Clean { apply: false },
            CommandMode::Vacuum { apply: false },
            CommandMode::Clean { apply: true },
            CommandMode::Vacuum { apply: true },
        ];

        for state in states {
            for command in commands {
                let fake = FakeInspector(state.clone());
                let (_, decision) =
                    inspect_and_decide(&fake, Path::new("opencode.db"), command, false);
                let destructive = matches!(
                    command,
                    CommandMode::Clean { apply: true } | CommandMode::Vacuum { apply: true }
                );
                match (&state.verdict, destructive) {
                    (Verdict::Held(_), true) => {
                        assert!(matches!(decision, GateDecision::RefuseHeld(_)));
                    }
                    (Verdict::CannotDetermine(_), true) => {
                        assert!(matches!(decision, GateDecision::RefuseCannotDetermine(_)));
                    }
                    (Verdict::NotHeld, _) => assert_eq!(decision, GateDecision::Proceed),
                    (Verdict::Held(_) | Verdict::CannotDetermine(_), false) => {
                        assert_eq!(decision, GateDecision::Warn);
                    }
                }
            }
        }
    }

    #[test]
    fn force_bypasses_both_destructive_refusals() {
        for state in [held(), unknown()] {
            let fake = FakeInspector(state);
            for command in [
                CommandMode::Clean { apply: true },
                CommandMode::Vacuum { apply: true },
            ] {
                let (_, decision) =
                    inspect_and_decide(&fake, Path::new("opencode.db"), command, true);
                assert_eq!(decision, GateDecision::Warn);
            }
        }
    }

    #[test]
    fn destructive_refusals_map_to_distinct_database_busy_errors() {
        let held_error = inspect_and_decide(
            &FakeInspector(held()),
            Path::new("opencode.db"),
            CommandMode::Clean { apply: true },
            false,
        )
        .1
        .into_result()
        .expect_err("held database must refuse cleanup");
        let unknown_error = inspect_and_decide(
            &FakeInspector(unknown()),
            Path::new("opencode.db"),
            CommandMode::Vacuum { apply: true },
            false,
        )
        .1
        .into_result()
        .expect_err("unknown holder state must refuse vacuum");

        assert_eq!(held_error.exit_code(), 5);
        assert_eq!(unknown_error.exit_code(), 5);
        assert!(held_error.to_string().contains("pid 42"));
        assert!(
            unknown_error
                .to_string()
                .contains("cannot determine process holders: permission denied")
        );
    }
}
