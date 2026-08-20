//! Linux `/proc` process-holder inspection.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use procfs::ProcError;
use procfs::process::{FDTarget, Process, all_processes_with_root};

use super::{
    Completeness, HolderInfo, HolderInspector, Inspection, Verdict, database_related_paths,
};

/// Linux inspector backed by procfs.
#[derive(Clone, Debug)]
pub struct LinuxHolderInspector {
    proc_root: PathBuf,
}

impl Default for LinuxHolderInspector {
    fn default() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
        }
    }
}

impl LinuxHolderInspector {
    /// Creates an inspector rooted at an alternate procfs mount.
    #[must_use]
    pub fn with_proc_root(proc_root: PathBuf) -> Self {
        Self { proc_root }
    }
}

impl HolderInspector for LinuxHolderInspector {
    fn inspect(&self, database_path: &Path) -> Inspection {
        let targets = database_related_paths(database_path);
        let processes = match all_processes_with_root(&self.proc_root) {
            Ok(processes) => processes,
            Err(error) => return root_error_inspection(&self.proc_root, &error),
        };
        let mut matches = BTreeMap::<u32, (Option<String>, Vec<PathBuf>)>::new();
        let mut partial = false;

        for process in processes {
            let process = match process {
                Ok(process) => process,
                Err(error) if process_vanished(&error) => continue,
                Err(error) => {
                    partial |= scan_was_blinded(&error);
                    continue;
                }
            };
            inspect_process(&process, &targets, &mut matches, &mut partial);
        }

        let holders = matches
            .into_iter()
            .map(|(pid, (process_name, matched_paths))| HolderInfo {
                pid,
                process_name,
                matched_paths,
            })
            .collect::<Vec<_>>();
        let completeness = if partial {
            Completeness::PartialDueToPermissions
        } else {
            Completeness::CompleteForVisibleProcesses
        };
        let verdict = if holders.is_empty() {
            if partial {
                Verdict::CannotDetermine(
                    "permission restrictions prevented inspection of every visible process"
                        .to_owned(),
                )
            } else {
                Verdict::NotHeld
            }
        } else {
            Verdict::Held(holders)
        };
        Inspection {
            verdict,
            completeness,
        }
    }
}

fn inspect_process(
    process: &Process,
    targets: &[PathBuf; 3],
    matches: &mut BTreeMap<u32, (Option<String>, Vec<PathBuf>)>,
    partial: &mut bool,
) {
    let fds = match process.fd() {
        Ok(fds) => fds,
        Err(error) if process_vanished(&error) => return,
        Err(error) => {
            *partial |= scan_was_blinded(&error);
            return;
        }
    };
    for fd in fds {
        let fd = match fd {
            Ok(fd) => fd,
            Err(error) if process_vanished(&error) => continue,
            Err(error) => {
                *partial |= scan_was_blinded(&error);
                continue;
            }
        };
        let FDTarget::Path(path) = fd.target else {
            continue;
        };
        let Some(target) = targets.iter().find(|target| *target == &path) else {
            continue;
        };
        let Ok(pid) = u32::try_from(process.pid) else {
            continue;
        };
        let entry = matches.entry(pid).or_insert_with(|| {
            let process_name = process.stat().ok().map(|stat| stat.comm);
            (process_name, Vec::new())
        });
        if !entry.1.contains(target) {
            entry.1.push(target.clone());
        }
    }
}

fn root_error_inspection(proc_root: &Path, error: &ProcError) -> Inspection {
    let completeness = match error {
        ProcError::PermissionDenied(_) => Completeness::PartialDueToPermissions,
        ProcError::Io(source, _) if source.kind() == ErrorKind::PermissionDenied => {
            Completeness::PartialDueToPermissions
        }
        _ => Completeness::Unsupported,
    };
    Inspection {
        verdict: Verdict::CannotDetermine(format!(
            "cannot enumerate {}: {error}",
            proc_root.display()
        )),
        completeness,
    }
}

fn process_vanished(error: &ProcError) -> bool {
    match error {
        ProcError::NotFound(_) => true,
        ProcError::Incomplete(Some(path)) => !path.exists(),
        ProcError::Io(source, _) => source.kind() == ErrorKind::NotFound,
        _ => false,
    }
}

fn scan_was_blinded(error: &ProcError) -> bool {
    !process_vanished(error)
}
