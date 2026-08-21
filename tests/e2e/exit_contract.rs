#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Coverage {
    E2e(&'static str),
    UnitOnly(&'static str),
}

const EXIT_CODE_COVERAGE: [(i32, Coverage); 12] = [
    (
        0,
        Coverage::E2e("analyze_reports_every_layer_in_human_and_json_forms"),
    ),
    (1, Coverage::E2e("malformed_database_produces_exit_one")),
    (
        2,
        Coverage::E2e("keep_recent_alone_exits_two_without_mutation"),
    ),
    (3, Coverage::E2e("missing_database_produces_exit_three")),
    (
        4,
        Coverage::E2e("incompatible_schema_produces_exit_four_without_mutation"),
    ),
    (5, Coverage::E2e("held_database_produces_exit_five")),
    (
        6,
        Coverage::E2e("clean_incremental_on_auto_vacuum_none_produces_exit_six"),
    ),
    (7, Coverage::E2e("foreign_key_failure_produces_exit_seven")),
    (
        8,
        Coverage::E2e("interrupt_after_committed_batch_produces_exit_eight"),
    ),
    (
        9,
        Coverage::E2e("unsupported_platform_binary_produces_exit_nine"),
    ),
    (
        10,
        Coverage::E2e("filesystem_cleanup_failure_produces_exit_ten"),
    ),
    (
        11,
        Coverage::UnitOnly("src/error.rs::error_exit_codes_match_stable_contract"),
    ),
];

#[test]
fn documented_exit_code_table_has_a_producing_scenario() {
    let codes = EXIT_CODE_COVERAGE.map(|(code, _)| code);
    assert_eq!(codes, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    assert!(
        EXIT_CODE_COVERAGE
            .iter()
            .all(|(_, coverage)| match coverage {
                Coverage::E2e(test) | Coverage::UnitOnly(test) => !test.is_empty(),
            })
    );
    assert_eq!(
        EXIT_CODE_COVERAGE
            .iter()
            .filter(|(_, coverage)| matches!(coverage, Coverage::UnitOnly(_)))
            .map(|(code, _)| *code)
            .collect::<Vec<_>>(),
        [11]
    );
}
