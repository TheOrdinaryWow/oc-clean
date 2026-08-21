use super::command::{
    inspect, join_independent_checks, run_independent_checks, run_independent_checks_sequential,
};
use crate::error::Error;
use crate::paths::{self, Target};

use std::path::Path;

#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

use fixture::{Fixture, FixtureConfig};

#[test]
fn parallel_checks_match_the_sequential_reference() {
    let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
    let fixture_target = Target::File(fixture.database_path.clone());
    let parallel_fixture = run_independent_checks(&fixture_target, &fixture.database_path)
        .expect("parallel checks should succeed on the file fixture");
    let sequential_fixture =
        run_independent_checks_sequential(&fixture_target, &fixture.database_path)
            .expect("sequential checks should succeed on the file fixture");
    assert_eq!(parallel_fixture, sequential_fixture);
    assert!(parallel_fixture.integrity_check.ok);
    assert!(parallel_fixture.foreign_key_check.ok);
    assert!(parallel_fixture.file_space.total_bytes > 0);

    let target = Target::Memory;
    let database_path = Path::new(":memory:");
    let derived_paths = paths::derived_paths(Path::new(paths::MEMORY_DATA_DIR));

    let parallel = inspect(
        &target,
        database_path,
        &derived_paths,
        run_independent_checks,
    )
    .expect("parallel checks should succeed on the memory fixture");
    let sequential = inspect(
        &target,
        database_path,
        &derived_paths,
        run_independent_checks_sequential,
    )
    .expect("sequential checks should succeed on the memory fixture");

    assert_eq!(parallel, sequential);

    let mut parallel_json = Vec::new();
    let mut sequential_json = Vec::new();
    super::json::write(&parallel, &mut parallel_json).expect("parallel JSON should render");
    super::json::write(&sequential, &mut sequential_json).expect("sequential JSON should render");
    assert_eq!(parallel_json, sequential_json);

    let style = crate::report::format::Style::resolve(false, false);
    let mut parallel_human = Vec::new();
    let mut sequential_human = Vec::new();
    super::human::write(&parallel, &mut parallel_human, style)
        .expect("parallel human report should render");
    super::human::write(&sequential, &mut sequential_human, style)
        .expect("sequential human report should render");
    assert_eq!(parallel_human, sequential_human);
}

#[test]
fn simultaneous_failures_always_surface_in_declaration_order() {
    for _ in 0..20 {
        let outcome = crate::parallel::group("doctor", 4, |group| {
            let integrity = group.spawn("integrity", || {
                Err(Error::IntegrityCheckFailed {
                    check: "integrity_check".to_owned(),
                    message: "first declared failure".to_owned(),
                })
            });
            let foreign_keys = group.spawn("foreign keys", || {
                Err(Error::InvalidArgument {
                    argument: "foreign keys".to_owned(),
                    reason: "second declared failure".to_owned(),
                })
            });
            let file_space = group.spawn("file space", || {
                Err(Error::InvalidArgument {
                    argument: "file space".to_owned(),
                    reason: "third declared failure".to_owned(),
                })
            });
            let holders = group.spawn("holders", || {
                Err(Error::UnsupportedPlatform {
                    platform: "fourth declared failure".to_owned(),
                })
            });

            join_independent_checks(integrity, foreign_keys, file_space, holders)
        });

        let error = outcome.expect_err("every check should fail");
        assert_eq!(error.exit_code(), 7);
        assert!(matches!(error, Error::IntegrityCheckFailed { .. }));
    }
}
