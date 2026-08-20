//! macOS libproc process-holder inspection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use libproc::bsd_info::BSDInfo;
use libproc::file_info::ListFDs;
use libproc::proc_pid::{listpidinfo, name, pidinfo};
use libproc::processes::{ProcFilter, pids_by_path, pids_by_type};

use super::{
    Completeness, HolderInfo, HolderInspector, Inspection, Verdict, database_related_paths,
};

/// macOS inspector backed by libproc.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacosHolderInspector;

impl HolderInspector for MacosHolderInspector {
    fn inspect(&self, database_path: &Path) -> Inspection {
        let visible_pids = match pids_by_type(ProcFilter::All) {
            Ok(pids) => pids,
            Err(error) => {
                return Inspection {
                    verdict: Verdict::CannotDetermine(format!(
                        "cannot enumerate processes with libproc: {error}"
                    )),
                    completeness: Completeness::Unsupported,
                };
            }
        };
        let visible = visible_pids.iter().copied().collect::<BTreeSet<_>>();
        let mut partial = false;
        for pid in &visible_pids {
            let process_id = *pid;
            let Ok(pid) = i32::try_from(process_id) else {
                partial = true;
                continue;
            };
            let Ok(info) = pidinfo::<BSDInfo>(pid, 0) else {
                partial |= process_still_exists(process_id);
                continue;
            };
            let max_files = usize::try_from(info.pbi_nfiles).unwrap_or(usize::MAX);
            if max_files == 0 {
                continue;
            }
            if listpidinfo::<ListFDs>(pid, max_files).is_err() {
                partial |= process_still_exists(process_id);
            }
        }

        let targets = database_related_paths(database_path);
        let mut matches = BTreeMap::<u32, Vec<PathBuf>>::new();
        for target in &targets {
            if !target.exists() {
                continue;
            }
            match pids_by_path(target, false, false) {
                Ok(pids) => {
                    for pid in pids {
                        if visible.contains(&pid) {
                            matches.entry(pid).or_default().push(target.clone());
                        }
                    }
                }
                Err(_) => partial = true,
            }
        }
        let holders = matches
            .into_iter()
            .map(|(pid, matched_paths)| HolderInfo {
                pid,
                process_name: i32::try_from(pid).ok().and_then(|pid| name(pid).ok()),
                matched_paths,
            })
            .collect::<Vec<_>>();
        let verdict = if holders.is_empty() {
            if partial {
                Verdict::CannotDetermine(
                    "libproc permissions prevented a complete holder scan".to_owned(),
                )
            } else {
                Verdict::NotHeld
            }
        } else {
            Verdict::Held(holders)
        };
        Inspection {
            verdict,
            completeness: if partial {
                Completeness::PartialDueToPermissions
            } else {
                Completeness::CompleteForVisibleProcesses
            },
        }
    }
}

fn process_still_exists(pid: u32) -> bool {
    pids_by_type(ProcFilter::All).is_ok_and(|pids| pids.contains(&pid))
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn detects_current_process_holding_fixture() {
        let fixture = NamedTempFile::new().expect("fixture");
        let _held = File::open(fixture.path()).expect("open fixture");

        let inspection = MacosHolderInspector.inspect(fixture.path());

        let Verdict::Held(holders) = inspection.verdict else {
            panic!("expected current process holder");
        };
        assert!(
            holders
                .iter()
                .any(|holder| holder.pid == std::process::id())
        );
    }
}
