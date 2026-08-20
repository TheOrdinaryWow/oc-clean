#[allow(dead_code)]
#[path = "support/fixture.rs"]
mod fixture;

mod doctor {
    use std::fs;
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
        assert_eq!(
            report["vacuum_headroom"]["estimated_required_bytes"],
            fixture.database_path.metadata().unwrap().len()
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

    fn metadata(path: &Path) -> (u64, std::time::SystemTime) {
        let metadata = path.metadata().unwrap();
        (metadata.len(), metadata.modified().unwrap())
    }
}
