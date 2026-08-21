#![cfg(feature = "bench-large")]
#![allow(dead_code)]

#[path = "support/fixture.rs"]
mod fixture;

use std::env;

use fixture::{Fixture, FixtureConfig, LargeFixtureError};

const DEFAULT_TARGET_BYTES: u64 = 2_000_000_000;
const TARGET_ENV: &str = "OC_CLEAN_BENCH_TARGET_BYTES";

fn requested_target_bytes() -> u64 {
    env::var(TARGET_ENV).map_or(DEFAULT_TARGET_BYTES, |value| {
        value
            .parse()
            .expect("target size must be an integer byte count")
    })
}

#[test]
#[ignore = "generates a multi-gigabyte fixture on demand"]
fn generator_builds_schema_accurate_database_near_target_size() {
    let target_bytes = requested_target_bytes();
    let fixture = Fixture::build(&FixtureConfig::bench_large(target_bytes))
        .expect("large fixture should build");
    let fixture_root = fixture.root().to_path_buf();
    let report = fixture
        .large_report()
        .expect("large fixture should include a generation report");
    let connection = fixture.connect().expect("large fixture should connect");
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity check should run");

    assert_eq!(integrity, "ok");
    assert_eq!(report.target_size_bytes, target_bytes);
    assert!(report.generation_time.as_nanos() > 0);
    assert!(
        report.achieved_size_bytes >= target_bytes.saturating_mul(9) / 10,
        "fixture is undersized: {report:?}"
    );
    assert!(
        report.achieved_size_bytes <= target_bytes.saturating_mul(11) / 10,
        "fixture is oversized: {report:?}"
    );
    assert!(report.session_count >= report.project_count);
    assert_eq!(report.message_count, report.session_count * 20);
    assert_eq!(report.part_count, report.message_count * 4);

    println!("{report:#?}");
    drop(connection);
    drop(fixture);
    assert!(
        !fixture_root.exists(),
        "temporary large fixture should be removed on drop"
    );
}

#[test]
#[ignore = "exercises the large-fixture capacity guard on demand"]
fn generator_rejects_target_larger_than_available_disk() {
    let Err(error) = Fixture::build(&FixtureConfig::bench_large(u64::MAX)) else {
        panic!("impossible target should fail before population");
    };
    let error = error
        .downcast_ref::<LargeFixtureError>()
        .expect("capacity failure should retain its typed error");

    assert!(matches!(
        error,
        LargeFixtureError::InsufficientDiskSpace { .. }
    ));
}
