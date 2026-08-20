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
}
