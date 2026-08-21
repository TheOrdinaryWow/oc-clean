//! Progress rendering for the clean pipeline.
//!
//! The pipeline advertises every phase it enters, which makes a phase-counting bar the
//! natural top-level indicator. Long phases additionally own a finer bar created directly
//! at the call site, because only the call site knows the unit of work being counted.

use std::cell::RefCell;

use crate::cli::CleanArgs;
use crate::report::progress::{self, Bar};

use super::{PhaseId, PhaseObserver};

/// The two working halves of the clean pipeline, separated by the phases that write to stdout.
///
/// One bar cannot span the whole pipeline. Reports and the confirmation prompt go to stdout
/// while the bar redraws on stderr, so a bar left alive across them interleaves its redraws
/// with the text the operator is trying to read. Each segment therefore owns a bar that is
/// cleared as soon as the pipeline reaches a phase that produces output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Segment {
    /// Selection and measurement, ending where the impact report is produced.
    Survey,
    /// Deletion and reclamation, entered only after the confirmation is answered.
    Execute,
}

impl Segment {
    /// Returns the segment a phase belongs to, or `None` for a phase that writes output.
    ///
    /// `P10` prepares the impact report, `P11` is the confirmation, and `P21` writes the final
    /// report. All three put text on stdout, so no bar may be drawn while they run.
    const fn of(phase: PhaseId) -> Option<Self> {
        match phase {
            PhaseId::P1
            | PhaseId::P2
            | PhaseId::P3
            | PhaseId::P3b
            | PhaseId::P4
            | PhaseId::P5
            | PhaseId::P6
            | PhaseId::P7
            | PhaseId::P8
            | PhaseId::P9 => Some(Self::Survey),
            PhaseId::P10 | PhaseId::P11 | PhaseId::P21 => None,
            PhaseId::P12
            | PhaseId::P13
            | PhaseId::P14
            | PhaseId::P15
            | PhaseId::P16
            | PhaseId::P17
            | PhaseId::P17b
            | PhaseId::P18
            | PhaseId::P19
            | PhaseId::P20 => Some(Self::Execute),
        }
    }

    const fn prefix(self) -> &'static str {
        match self {
            Self::Survey => "survey",
            Self::Execute => "execute",
        }
    }
}

/// Drives one phase bar per segment from the pipeline's own phase notifications.
pub(super) struct ProgressPhaseObserver {
    survey_length: u64,
    execute_length: u64,
    active: RefCell<Option<(Segment, Bar, u64)>>,
}

impl ProgressPhaseObserver {
    pub(super) fn new(arguments: &CleanArgs, apply: bool) -> Self {
        Self {
            survey_length: survey_phase_count(arguments, apply),
            execute_length: execute_phase_count(arguments, apply),
            active: RefCell::new(None),
        }
    }

    /// Clears whichever bar is drawn so the report is the only remaining terminal output.
    pub(super) fn finish(&self) {
        if let Some((_, bar, _)) = self.active.borrow_mut().take() {
            bar.finish();
        }
    }

    const fn length_of(&self, segment: Segment) -> u64 {
        match segment {
            Segment::Survey => self.survey_length,
            Segment::Execute => self.execute_length,
        }
    }
}

impl PhaseObserver for ProgressPhaseObserver {
    fn entered(&self, phase: PhaseId) {
        let Some(segment) = Segment::of(phase) else {
            // An output phase owns the terminal. Clearing here is what keeps reports and the
            // confirmation prompt free of redraws.
            self.finish();
            return;
        };
        let mut active = self.active.borrow_mut();
        if active
            .as_ref()
            .is_none_or(|(current, ..)| *current != segment)
        {
            if let Some((_, bar, _)) = active.take() {
                bar.finish();
            }
            *active = Some((
                segment,
                progress::phases(segment.prefix(), self.length_of(segment)),
                0,
            ));
        }
        let Some((_, bar, entered)) = active.as_mut() else {
            return;
        };
        *entered = entered.saturating_add(1);
        // A dry run stops early, and an interrupt stops earlier still, so the planned count
        // is an upper bound rather than a promise. Growing the bar keeps the ratio honest
        // instead of rendering a position beyond its length.
        bar.set_position(*entered);
        bar.set_message(match phase {
            PhaseId::P2 | PhaseId::P3 | PhaseId::P3b | PhaseId::P4 => "running independent checks",
            _ => phase.label(),
        });
    }
}

/// Returns how many phases the survey segment (P1 through P9) will enter.
///
/// P5 acquires the exclusive lock and only a mutating run reaches it; P3b is entered only when
/// incremental reclaim was requested.
fn survey_phase_count(arguments: &CleanArgs, apply: bool) -> u64 {
    let mut count: u64 = 8; // P1, P2, P3, P4, P6, P7, P8, P9
    if apply {
        count += 1; // P5 exclusive lock
    }
    if arguments.incremental {
        count += 1; // P3b auto-vacuum precondition
    }
    count
}

/// Returns how many phases the execute segment (P12 through P20) will enter.
///
/// A dry run never reaches this segment, so its count is zero. For a mutating run the count is
/// exact unless an interrupt stops the pipeline early, which makes it an upper bound.
fn execute_phase_count(arguments: &CleanArgs, apply: bool) -> u64 {
    if !apply {
        return 0;
    }
    let mut count: u64 = 5; // P12, P14, P15, P16, P17
    if arguments.orphans {
        count += 2; // P13 orphan deletion and P17b pre-existing orphan sweep
    }
    if arguments.gc_snapshots {
        count += 1; // P18 snapshot gc
    }
    if !arguments.incremental && !arguments.no_vacuum {
        count += 1; // P19 rebuild headroom
    }
    if !arguments.no_vacuum {
        count += 1; // P20 reclaim
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
    use super::{Segment, execute_phase_count, survey_phase_count};
    use crate::clean::PhaseId;
    use crate::cli::CleanArgs;

    fn arguments() -> CleanArgs {
        CleanArgs {
            older_than: None,
            include: None,
            exclude: None,
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
        assert_eq!(survey_phase_count(&arguments(), false), 8);
        assert_eq!(execute_phase_count(&arguments(), false), 0);

        let mut incremental = arguments();
        incremental.incremental = true;
        assert_eq!(survey_phase_count(&incremental, false), 9);
        assert_eq!(execute_phase_count(&incremental, false), 0);
    }

    #[test]
    fn a_mutating_run_adds_the_lock_phase_to_the_survey_segment() {
        assert_eq!(
            survey_phase_count(&arguments(), true),
            survey_phase_count(&arguments(), false) + 1
        );
    }

    #[test]
    fn optional_work_adds_its_own_phases() {
        let baseline = execute_phase_count(&arguments(), true);

        let mut orphans = arguments();
        orphans.orphans = true;
        assert_eq!(execute_phase_count(&orphans, true), baseline + 2);

        let mut gc = arguments();
        gc.gc_snapshots = true;
        assert_eq!(execute_phase_count(&gc, true), baseline + 1);

        let mut no_vacuum = arguments();
        no_vacuum.no_vacuum = true;
        assert_eq!(execute_phase_count(&no_vacuum, true), baseline - 2);
    }

    #[test]
    fn output_phases_belong_to_no_segment() {
        assert_eq!(Segment::of(PhaseId::P9), Some(Segment::Survey));
        assert_eq!(Segment::of(PhaseId::P10), None);
        assert_eq!(Segment::of(PhaseId::P11), None);
        assert_eq!(Segment::of(PhaseId::P12), Some(Segment::Execute));
        assert_eq!(Segment::of(PhaseId::P20), Some(Segment::Execute));
        assert_eq!(Segment::of(PhaseId::P21), None);
    }

    #[test]
    fn the_two_segments_partition_every_phase_except_the_confirmation() {
        let mut survey = 0;
        let mut execute = 0;
        let mut unsegmented = Vec::new();
        for phase in ALL_PHASES {
            match Segment::of(phase) {
                Some(Segment::Survey) => survey += 1,
                Some(Segment::Execute) => execute += 1,
                None => unsegmented.push(phase),
            }
        }

        // Exactly the three phases that write to stdout own no bar.
        assert_eq!(
            unsegmented,
            vec![PhaseId::P10, PhaseId::P11, PhaseId::P21],
            "only output phases may suppress the bar"
        );
        assert_eq!(survey + execute + unsegmented.len(), ALL_PHASES.len());

        // The planned counts never promise more than the segment actually contains.
        let mut everything = arguments();
        everything.orphans = true;
        everything.gc_snapshots = true;
        everything.incremental = true;
        assert!(survey_phase_count(&everything, true) <= survey as u64);
        assert!(execute_phase_count(&everything, true) <= execute as u64);
    }

    /// Every phase the pipeline defines, in pipeline order.
    const ALL_PHASES: [PhaseId; 23] = [
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
    ];

    #[test]
    fn every_phase_has_a_non_empty_label() {
        for phase in ALL_PHASES {
            assert!(!phase.label().is_empty(), "{phase} needs a label");
        }
    }
}
