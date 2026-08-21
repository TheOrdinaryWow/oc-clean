//! Progress rendering for the clean pipeline.
//!
//! The pipeline advertises every phase it enters, which makes a phase-counting bar the
//! natural top-level indicator. Long phases additionally own a finer bar created directly
//! at the call site, because only the call site knows the unit of work being counted.

use std::cell::RefCell;

use crate::cli::CleanArgs;
use crate::report::progress::{self, Bar};

use super::{PhaseId, PhaseObserver};

/// Drives the top-level phase bar from the pipeline's own phase notifications.
pub(super) struct ProgressPhaseObserver {
    bar: Bar,
    entered: RefCell<u64>,
}

impl ProgressPhaseObserver {
    pub(super) fn new(arguments: &CleanArgs, apply: bool) -> Self {
        Self {
            bar: progress::phases("clean", planned_phase_count(arguments, apply)),
            entered: RefCell::new(0),
        }
    }

    /// Clears the bar so the report is the only remaining terminal output.
    pub(super) fn finish(&self) {
        self.bar.finish();
    }
}

impl PhaseObserver for ProgressPhaseObserver {
    fn entered(&self, phase: PhaseId) {
        let mut entered = self.entered.borrow_mut();
        *entered = entered.saturating_add(1);
        // A dry run stops early, and an interrupt stops earlier still, so the planned count
        // is an upper bound rather than a promise. Growing the bar keeps the ratio honest
        // instead of rendering a position beyond its length.
        self.bar.set_position(*entered);
        self.bar.set_message(match phase {
            PhaseId::P2 | PhaseId::P3 | PhaseId::P3b | PhaseId::P4 => "running independent checks",
            _ => phase.label(),
        });
    }
}

/// Returns how many phases the pipeline will enter for this invocation.
///
/// The count is exact for a completed applied run, and an upper bound for a dry run or an
/// interrupted run, which both stop before the pipeline reaches its later phases.
fn planned_phase_count(arguments: &CleanArgs, apply: bool) -> u64 {
    if !apply {
        // P1 through P10, minus P5 which only an applied run acquires.
        return if arguments.incremental { 10 } else { 9 };
    }
    let mut count: u64 = 19;
    if arguments.incremental {
        count += 1; // P3b auto-vacuum precondition
        count -= 1; // P19 headroom is skipped
    }
    if arguments.no_vacuum {
        count -= 2; // P19 headroom and P20 reclaim
    }
    if arguments.orphans {
        count += 2; // P13 orphan deletion and P17b pre-existing orphan sweep
    }
    if arguments.gc_snapshots {
        count += 1; // P18 snapshot gc
    }
    count
}

impl PhaseId {
    /// Returns the short human description rendered beside the phase bar.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::P1 => "resolving database paths",
            Self::P2 => "inspecting schema",
            Self::P3 => "scanning for database holders",
            Self::P3b => "checking incremental auto-vacuum",
            Self::P4 => "validating selectors",
            Self::P5 => "acquiring the exclusive lock",
            Self::P6 => "computing the retention set",
            Self::P7 => "selecting candidate sessions",
            Self::P8 => "expanding descendant subtrees",
            Self::P9 => "summarizing impact",
            Self::P10 => "preparing the report",
            Self::P11 => "awaiting confirmation",
            Self::P12 => "deleting sessions",
            Self::P13 => "deleting orphans",
            Self::P14 => "pruning empty projects",
            Self::P15 => "verifying integrity",
            Self::P16 => "sweeping storage for this run",
            Self::P17 => "removing pruned snapshots",
            Self::P17b => "sweeping pre-existing orphans",
            Self::P18 => "compacting snapshot repositories",
            Self::P19 => "checking rebuild headroom",
            Self::P20 => "reclaiming database space",
            Self::P21 => "writing the final report",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::planned_phase_count;
    use crate::cli::CleanArgs;

    fn arguments() -> CleanArgs {
        CleanArgs {
            older_than: None,
            project: None,
            larger_than: None,
            archived: true,
            orphans: false,
            keep_recent: 0,
            incremental: false,
            no_vacuum: false,
            gc_snapshots: false,
            prune_empty_projects: false,
            top: 10,
            json: false,
        }
    }

    #[test]
    fn a_dry_run_counts_only_the_phases_it_reaches() {
        assert_eq!(planned_phase_count(&arguments(), false), 9);
        let mut incremental = arguments();
        incremental.incremental = true;
        assert_eq!(planned_phase_count(&incremental, false), 10);
    }

    #[test]
    fn optional_work_adds_its_own_phases() {
        let baseline = planned_phase_count(&arguments(), true);

        let mut orphans = arguments();
        orphans.orphans = true;
        assert_eq!(planned_phase_count(&orphans, true), baseline + 2);

        let mut gc = arguments();
        gc.gc_snapshots = true;
        assert_eq!(planned_phase_count(&gc, true), baseline + 1);

        let mut no_vacuum = arguments();
        no_vacuum.no_vacuum = true;
        assert_eq!(planned_phase_count(&no_vacuum, true), baseline - 2);
    }

    #[test]
    fn every_phase_has_a_non_empty_label() {
        use crate::clean::PhaseId;

        for phase in [
            PhaseId::P1,
            PhaseId::P2,
            PhaseId::P3,
            PhaseId::P3b,
            PhaseId::P4,
            PhaseId::P5,
            PhaseId::P6,
            PhaseId::P7,
            PhaseId::P8,
            PhaseId::P9,
            PhaseId::P10,
            PhaseId::P11,
            PhaseId::P12,
            PhaseId::P13,
            PhaseId::P14,
            PhaseId::P15,
            PhaseId::P16,
            PhaseId::P17,
            PhaseId::P17b,
            PhaseId::P18,
            PhaseId::P19,
            PhaseId::P20,
            PhaseId::P21,
        ] {
            assert!(!phase.label().is_empty(), "{phase} needs a label");
        }
    }
}
