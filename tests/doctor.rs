#[allow(dead_code)]
#[path = "support/fixture.rs"]
mod fixture;

mod doctor {
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;
    use std::process::{Command, Output};

    use rusqlite::params;
    use serde_json::Value;

    use super::fixture::{Fixture, FixtureConfig};

    #[path = "failures.rs"]
    mod failures;

    fn run(fixture: &Fixture, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oc-clean"))
            .arg("doctor")
            .arg("--db")
            .arg(&fixture.database_path)
            .args(arguments)
            .env_remove("NO_COLOR")
            .output()
            .expect("oc-clean doctor should run")
    }

    fn json(output: &Output) -> Value {
        serde_json::from_slice(&output.stdout).expect("stdout should contain one JSON object")
    }

    fn insert_orphan_events(fixture: &Fixture, count: usize) {
        let connection = fixture.connect().expect("fixture should connect");
        for index in 0..count {
            let aggregate_id = format!("ses_DoctorOrphan{index}");
            connection
                .execute(
                    "INSERT INTO event_sequence VALUES (?1, 1, NULL)",
                    [&aggregate_id],
                )
                .expect("orphan event sequence should insert");
            connection
                .execute(
                    "INSERT INTO event VALUES (?1, ?2, 1, 'session.orphan', '{}')",
                    params![format!("doctor-orphan-{index}"), aggregate_id],
                )
                .expect("orphan event should insert");
        }
    }

    #[test]
    fn json_reports_every_health_section_and_orphan_class() {
        let fixture = Fixture::build(&FixtureConfig {
            dangling_parent_session_count: 3,
            orphan_storage_file_count: 5,
            orphan_snapshot_dir_count: 2,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        insert_orphan_events(&fixture, 7);

        let output = run(&fixture, &["--json"]);

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = json(&output);
        assert_eq!(report["schema_version"], 1);
        for tier in ["tier_one", "tier_two", "tier_three"] {
            assert!(report["schema"][tier]["findings"].is_array());
        }
        assert_eq!(report["integrity_check"]["ok"], true);
        assert_eq!(
            report["integrity_check"]["findings"],
            serde_json::json!(["ok"])
        );
        assert_eq!(report["foreign_key_check"]["ok"], true);
        assert!(report["foreign_key_check"]["findings"].is_array());
        assert_eq!(report["orphans"]["orphan_events"]["count"], 7);
        assert_eq!(report["orphans"]["dangling_parent_sessions"]["count"], 3);
        assert_eq!(report["orphans"]["orphan_storage_files"]["count"], 5);
        assert_eq!(report["orphans"]["orphan_snapshot_directories"]["count"], 2);
        assert!(matches!(
            report["holders"]["completeness"].as_str(),
            Some("complete" | "partial-due-to-permissions" | "unsupported")
        ));
        assert!(matches!(
            report["holders"]["completeness_discriminant"].as_str(),
            Some("CompleteForVisibleProcesses" | "PartialDueToPermissions" | "Unsupported")
        ));
        assert!(
            report["holders"]["snapshot_warning"]
                .as_str()
                .unwrap()
                .contains("point-in-time")
        );
        for holder in report["holders"]["processes"].as_array().unwrap() {
            assert!(holder["pid"].is_u64());
            assert!(holder["name"].is_string() || holder["name"].is_null());
            assert!(holder["observed_via"].is_string());
        }
        assert!(report["vacuum_headroom"]["estimated_required_bytes"].is_u64());
        assert!(
            report["vacuum_headroom"]["estimated_required_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(report["vacuum_headroom"]["available_bytes"].is_u64());
        assert!(report["vacuum_headroom"]["vacuum_into_feasible"].is_boolean());
        assert!(report["auto_vacuum"]["value"].is_u64());
        assert!(report["auto_vacuum"]["mode"].is_string());
        assert_eq!(report["timestamp_sanity"]["ok"], true);
        assert_eq!(
            report["timestamp_sanity"]["classification"],
            "plausible-millisecond-epoch"
        );
    }

    #[test]
    fn schema_findings_are_rendered_verbatim_in_json() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        fixture
            .connect()
            .unwrap()
            .execute_batch(
                "ALTER TABLE session ADD COLUMN doctor_extension TEXT;
                 CREATE INDEX doctor_extension_idx ON session(doctor_extension);
                 CREATE TRIGGER doctor_delete_trigger
                 AFTER DELETE ON session BEGIN SELECT 1; END;",
            )
            .unwrap();

        let output = run(&fixture, &["--json"]);

        assert!(output.status.success());
        let report = json(&output);
        assert!(
            report["schema"]["tier_two"]["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding == "unknown column `session.doctor_extension`")
        );
        assert!(
            report["schema"]["tier_two"]["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding == "unknown index `doctor_extension_idx`")
        );
        assert!(
            report["schema"]["tier_three"]["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding.as_str().unwrap().contains("doctor_delete_trigger"))
        );
    }

    #[test]
    fn human_report_names_each_section_and_holder_scan_limit() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");

        let output = run(&fixture, &[]);

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        for heading in [
            "Schema Compatibility",
            "Integrity Check",
            "Foreign Key Check",
            "Orphan Census",
            "Database Holders",
            "VACUUM Headroom",
            "Auto Vacuum",
            "Timestamp Sanity",
        ] {
            assert!(
                stdout.contains(heading),
                "missing heading {heading}: {stdout}"
            );
        }
        assert!(stdout.contains("point-in-time"));
        assert!(!stdout.as_bytes().contains(&0x1b));
    }

    #[test]
    fn read_only_permission_fixture_succeeds_without_database_changes() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let before = metadata(&fixture.database_path);
        let mut permissions = fs::metadata(&fixture.database_path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&fixture.database_path, permissions).unwrap();

        let output = run(&fixture, &["--json"]);

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(metadata(&fixture.database_path), before);
    }

    #[test]
    fn corrupted_btree_page_returns_integrity_exit_code() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 512,
            messages_per_session: 0,
            parts_per_message: 0,
            ..FixtureConfig::default()
        })
        .expect("fixture should build");
        let connection = fixture.connect().expect("fixture should connect");
        let (page_size, leaf_page): (i64, i64) = connection
            .query_row(
                "SELECT (SELECT page_size FROM pragma_page_size), pageno FROM dbstat WHERE name = 'session' AND pagetype = 'leaf' AND pageno != (SELECT rootpage FROM sqlite_master WHERE type = 'table' AND name = 'session') ORDER BY pageno DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("non-root session leaf page should be found");
        drop(connection);
        let mut database = OpenOptions::new()
            .write(true)
            .open(&fixture.database_path)
            .expect("fixture database should open for corruption");
        database
            .seek(SeekFrom::Start(
                u64::try_from((leaf_page - 1) * page_size)
                    .expect("page offset should be non-negative"),
            ))
            .expect("session leaf page should be seekable");
        database
            .write_all(&[0])
            .expect("session leaf page should be corrupted");
        database.sync_all().expect("corruption should be persisted");

        let output = run(&fixture, &[]);

        assert_eq!(output.status.code(), Some(7));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("integrity"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn missing_required_schema_column_returns_schema_exit_code() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        fixture
            .connect()
            .expect("fixture should connect")
            .execute_batch(
                "ALTER TABLE session RENAME COLUMN time_updated TO incompatible_time_updated;
                 CREATE TABLE doctor_fk (
                    id TEXT PRIMARY KEY,
                    session_id TEXT REFERENCES session(id)
                 );
                 PRAGMA foreign_keys = OFF;
                 INSERT INTO doctor_fk VALUES ('broken', 'ses_missing');",
            )
            .expect("required column should be renamed");

        let output = run(&fixture, &["--json"]);

        assert_eq!(output.status.code(), Some(4));
        assert!(
            output.stdout.is_empty(),
            "schema gate must suppress the report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("schema is incompatible"),
            "stderr: {stderr}"
        );
        assert!(!stderr.contains("foreign_key_check"));
        assert!(!stderr.contains("integrity_check"));
    }

    fn metadata(path: &Path) -> (u64, std::time::SystemTime) {
        let metadata = path.metadata().unwrap();
        (metadata.len(), metadata.modified().unwrap())
    }
}
