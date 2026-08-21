#[allow(dead_code)]
#[path = "support/fixture.rs"]
mod fixture;

mod analyze {
    mod space {
        use std::collections::BTreeSet;
        use std::ffi::OsString;
        use std::fs;
        use std::path::{Path, PathBuf};

        use super::super::fixture::{Fixture, FixtureConfig};
        use oc_clean::analyze::space::{
            AccountingMethod, ObjectKind, analyze, analyze_with_capabilities,
        };
        use oc_clean::db::{Capabilities, ConnectionOptions, open_read_only};
        use oc_clean::paths::Target;

        const FALLBACK_TABLES: [&str; 5] = [
            "event",
            "message",
            "part",
            "session_context_epoch",
            "session_message",
        ];

        fn open_fixture(fixture: &Fixture) -> oc_clean::db::ReadOnlyConnection {
            open_read_only(
                &Target::File(fixture.database_path.clone()),
                ConnectionOptions::default(),
            )
            .expect("fixture should open read-only")
        }

        fn sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
            let mut path = OsString::from(database_path.as_os_str());
            path.push(suffix);
            PathBuf::from(path)
        }

        fn forced_without_dbstat(capabilities: &Capabilities) -> Capabilities {
            Capabilities {
                dbstat: false,
                ..capabilities.clone()
            }
        }

        #[test]
        fn file_pages_account_for_database_within_one_page_and_stat_sidecars() {
            let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
            let database = open_fixture(&fixture);
            let wal_path = sidecar_path(&fixture.database_path, "-wal");
            let shm_path = sidecar_path(&fixture.database_path, "-shm");
            fs::write(&wal_path, [0_u8; 17]).unwrap();
            fs::write(&shm_path, [0_u8; 11]).unwrap();

            let report = analyze(&database).unwrap();
            let database_bytes = fs::metadata(&fixture.database_path).unwrap().len();
            let accounted_bytes = report.file.live_bytes + report.file.freelist_bytes;

            assert_eq!(accounted_bytes, report.file.total_bytes);
            assert!(accounted_bytes.abs_diff(database_bytes) <= u64::from(report.file.page_size));
            assert_eq!(
                report.file.wal_bytes,
                Some(fs::metadata(wal_path).unwrap().len())
            );
            assert_eq!(
                report.file.shm_bytes,
                Some(fs::metadata(shm_path).unwrap().len())
            );
            assert!((0.0..=100.0).contains(&report.file.freelist_percent));
        }

        #[test]
        fn dbstat_reports_every_table_and_indexes_from_sqlite_master() {
            let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
            let database = open_fixture(&fixture);
            assert!(database.capabilities().dbstat);

            let report = analyze(&database).unwrap();
            let expected_tables = database
                .connection()
                .prepare(
                    "SELECT name FROM sqlite_master \
                     WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                )
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<BTreeSet<_>, _>>()
                .unwrap();
            let reported_tables = report
                .objects
                .entries
                .iter()
                .filter(|entry| entry.kind == ObjectKind::Table)
                .map(|entry| entry.name.clone())
                .collect::<BTreeSet<_>>();

            assert_eq!(report.objects.method, AccountingMethod::Dbstat);
            assert!(!report.objects.is_estimate());
            assert!(expected_tables.is_subset(&reported_tables));
            assert!(
                report
                    .objects
                    .entries
                    .iter()
                    .any(|entry| entry.kind == ObjectKind::Index)
            );
        }

        #[test]
        fn dbstat_part_bytes_are_within_five_percent_of_known_blob_total() {
            let config = FixtureConfig {
                session_count: 2,
                messages_per_session: 2,
                parts_per_message: 4,
                blob_size_per_part: 65_536,
                ..FixtureConfig::default()
            };
            let fixture = Fixture::build(&config).unwrap();
            let database = open_fixture(&fixture);

            let report = analyze(&database).unwrap();
            let part_bytes = report
                .objects
                .entries
                .iter()
                .find(|entry| entry.name == "part" && entry.kind == ObjectKind::Table)
                .expect("dbstat should report the part table")
                .bytes;
            let known_blob_bytes = u64::try_from(
                config.session_count
                    * config.messages_per_session
                    * config.parts_per_message
                    * config.blob_size_per_part,
            )
            .unwrap();

            assert!(part_bytes.abs_diff(known_blob_bytes) * 100 <= known_blob_bytes * 5);
        }

        #[test]
        fn octet_length_fallback_reports_five_tables_as_an_estimate() {
            let config = FixtureConfig {
                blob_size_per_part: 4_096,
                ..FixtureConfig::default()
            };
            let fixture = Fixture::build(&config).unwrap();
            let database = open_fixture(&fixture);
            let capabilities = forced_without_dbstat(database.capabilities());

            let report = analyze_with_capabilities(&database, &capabilities).unwrap();
            let reported_tables = report
                .objects
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<BTreeSet<_>>();

            assert_eq!(report.objects.method, AccountingMethod::OctetLengthEstimate);
            assert!(report.objects.is_estimate());
            assert!(report.objects.label.contains("estimate"));
            assert_eq!(
                reported_tables,
                FALLBACK_TABLES.into_iter().collect::<BTreeSet<_>>()
            );
            assert!(
                report
                    .objects
                    .entries
                    .iter()
                    .all(|entry| entry.kind == ObjectKind::Table)
            );
        }

        #[test]
        fn fallback_sql_uses_octet_length_for_data_columns() {
            let source = include_str!("../src/analyze/space.rs");

            assert!(source.contains("octet_length("));
            assert!(!source.contains(" length("));
            assert!(!source.contains(" LENGTH("));
        }
    }

    mod attribution {
        use std::sync::mpsc;
        use std::time::Duration;

        use super::super::fixture::{Fixture, FixtureConfig};
        use oc_clean::analyze::attribution::{AttributionReport, analyze};
        use oc_clean::db::{ConnectionOptions, open_read_only};
        use oc_clean::error::Error;
        use oc_clean::paths::Target;
        use rusqlite::params;

        const FIXTURE_SELF_BYTES: u64 = 21;

        fn open_fixture(fixture: &Fixture) -> oc_clean::db::ReadOnlyConnection {
            open_read_only(
                &Target::File(fixture.database_path.clone()),
                ConnectionOptions::default(),
            )
            .expect("fixture should open read-only")
        }

        fn report(config: &FixtureConfig, top_n: usize) -> (Fixture, AttributionReport) {
            let fixture = Fixture::build(config).unwrap();
            let database = open_fixture(&fixture);
            let report = analyze(&database, top_n).unwrap();
            (fixture, report)
        }

        #[test]
        fn parent_size_includes_its_entire_descendant_subtree() {
            let config = FixtureConfig {
                session_count: 1,
                sub_session_depth: 1,
                sub_session_fan_out: 2,
                ..FixtureConfig::default()
            };
            let (_fixture, report) = report(&config, 3);
            let root = report
                .sessions
                .iter()
                .find(|entry| entry.session_id == "ses_0")
                .unwrap();

            assert_eq!(root.self_bytes, FIXTURE_SELF_BYTES);
            assert_eq!(root.subtree_bytes, FIXTURE_SELF_BYTES * 3);
        }

        #[test]
        fn three_level_chain_is_fully_accumulated_into_root() {
            let config = FixtureConfig {
                session_count: 1,
                sub_session_depth: 3,
                sub_session_fan_out: 1,
                ..FixtureConfig::default()
            };
            let (_fixture, report) = report(&config, 4);
            let root = report
                .sessions
                .iter()
                .find(|entry| entry.session_id == "ses_0")
                .unwrap();

            assert_eq!(root.subtree_bytes, FIXTURE_SELF_BYTES * 4);
        }

        #[test]
        fn project_totals_equal_the_sum_of_session_self_sizes() {
            let config = FixtureConfig {
                project_count: 3,
                session_count: 6,
                ..FixtureConfig::default()
            };
            let (_fixture, report) = report(&config, 6);

            for project in &report.projects {
                let session_sum = report
                    .sessions
                    .iter()
                    .filter(|session| session.project_id == project.project_id)
                    .map(|session| session.self_bytes)
                    .sum::<u64>();
                assert_eq!(project.bytes, session_sum, "{}", project.project_id);
            }
        }

        #[test]
        fn top_n_limits_equal_sized_sessions_by_identifier() {
            let config = FixtureConfig {
                session_count: 3,
                ..FixtureConfig::default()
            };
            let (_fixture, report) = report(&config, 2);

            assert_eq!(
                report
                    .sessions
                    .iter()
                    .map(|entry| entry.session_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["ses_0", "ses_1"]
            );
        }

        #[test]
        fn three_projects_are_sorted_by_known_attributed_bytes() {
            let config = FixtureConfig {
                project_count: 3,
                session_count: 3,
                ..FixtureConfig::default()
            };
            let fixture = Fixture::build(&config).unwrap();
            let connection = fixture.connect().unwrap();
            for (session_id, payload_bytes) in [("ses_0", 30), ("ses_1", 10), ("ses_2", 20)] {
                connection
                    .execute(
                        "UPDATE part SET data = zeroblob(?1) WHERE session_id = ?2",
                        params![payload_bytes, session_id],
                    )
                    .unwrap();
            }
            drop(connection);
            let database = open_fixture(&fixture);

            let report = analyze(&database, 3).unwrap();

            assert_eq!(
                report
                    .projects
                    .iter()
                    .map(|entry| (entry.project_id.as_str(), entry.bytes))
                    .collect::<Vec<_>>(),
                vec![("project-0", 40), ("project-2", 30), ("project-1", 20)]
            );
        }

        #[test]
        fn parent_cycle_terminates_and_returns_a_typed_error() {
            let fixture = Fixture::build(&FixtureConfig {
                session_count: 2,
                ..FixtureConfig::default()
            })
            .unwrap();
            let connection = fixture.connect().unwrap();
            connection
                .execute(
                    "UPDATE session SET parent_id = CASE id WHEN 'ses_0' THEN 'ses_1' ELSE 'ses_0' END",
                    [],
                )
                .unwrap();
            drop(connection);
            let database_path = fixture.database_path.clone();
            let (sender, receiver) = mpsc::channel();

            std::thread::spawn(move || {
                let database =
                    open_read_only(&Target::File(database_path), ConnectionOptions::default())
                        .unwrap();
                sender.send(analyze(&database, 2)).unwrap();
            });

            let result = receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("cycle detection should finish within two seconds");
            match result {
                Err(Error::SchemaIncompatible { incompatibility }) => {
                    assert!(incompatibility.contains("session.parent_id cycle"));
                }
                other => panic!("expected parent cycle error, got {other:?}"),
            }
        }

        #[test]
        fn each_payload_table_has_one_grouped_scan_for_both_rollups() {
            let source = include_str!("../src/analyze/attribution.rs");
            // Only the rollup query is scan-sensitive: it aggregates every payload table across
            // the whole database. Per-session lookups elsewhere in the module are indexed point
            // reads and are excluded from this budget on purpose.
            let rollup_start = source
                .find("const ATTRIBUTION_SQL")
                .expect("the rollup query constant should exist");
            let rollup_end = source[rollup_start..]
                .find("\";")
                .expect("the rollup query constant should terminate")
                + rollup_start;
            let rollup = &source[rollup_start..rollup_end];

            for table in [
                "message",
                "part",
                "session_context_epoch",
                "session_message",
                "event",
            ] {
                assert_eq!(
                    rollup.matches(&format!("FROM {table} ")).count(),
                    1,
                    "{table}"
                );
            }
            assert!(rollup.contains("octet_length("));
            assert!(!source.contains(" LENGTH("));
        }
    }
}
