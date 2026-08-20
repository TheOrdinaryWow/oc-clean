use std::path::PathBuf;

use super::*;
use crate::safety::holders::HolderInfo;

#[test]
fn timestamp_classifier_names_milliseconds_seconds_outliers_and_empty_tables() {
    assert_eq!(
        classify_timestamp(Some(YEAR_2020_MILLISECONDS)).classification,
        "plausible-millisecond-epoch"
    );
    assert_eq!(
        classify_timestamp(Some(YEAR_2020_SECONDS)).classification,
        "likely-second-epoch"
    );
    assert_eq!(
        classify_timestamp(Some(42)).classification,
        "implausible-epoch"
    );
    assert_eq!(
        classify_timestamp(None).classification,
        "empty-session-table"
    );
}

#[test]
fn holder_report_preserves_all_completeness_discriminants() {
    let held = holder_report(Inspection {
        verdict: Verdict::Held(vec![HolderInfo {
            pid: 42,
            process_name: Some("fixture-holder".to_owned()),
            matched_paths: vec![PathBuf::from("fixture.db")],
        }]),
        completeness: Completeness::PartialDueToPermissions,
    });
    assert_eq!(held.verdict, "held");
    assert_eq!(held.completeness, "partial-due-to-permissions");
    assert_eq!(held.completeness_discriminant, "PartialDueToPermissions");
    assert_eq!(held.processes[0].pid, 42);

    let unknown = holder_report(Inspection {
        verdict: Verdict::CannotDetermine("unavailable".to_owned()),
        completeness: Completeness::Unsupported,
    });
    assert_eq!(unknown.verdict, "cannot-determine");
    assert_eq!(unknown.completeness_discriminant, "Unsupported");
    assert_eq!(unknown.reason.as_deref(), Some("unavailable"));

    let clear = holder_report(Inspection {
        verdict: Verdict::NotHeld,
        completeness: Completeness::CompleteForVisibleProcesses,
    });
    assert_eq!(clear.verdict, "not-held-at-scan-time");
    assert_eq!(
        clear.completeness_discriminant,
        "CompleteForVisibleProcesses"
    );
}
