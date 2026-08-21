#[allow(dead_code)]
#[path = "support/fixture.rs"]
mod fixture;

mod report {
    use std::process::{Command, Output};

    use serde_json::Value;

    use super::fixture::{Fixture, FixtureConfig};

    fn run(fixture: &Fixture, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oc-clean"))
            .arg("analyze")
            .arg("--db")
            .arg(&fixture.database_path)
            .args(arguments)
            .env_remove("NO_COLOR")
            .output()
            .expect("oc-clean should run")
    }

    fn stdout(output: &Output) -> String {
        String::from_utf8(output.stdout.clone()).expect("stdout should be UTF-8")
    }

    #[test]
    fn human_and_json_reports_cross_check_three_fields() {
        let fixture = Fixture::build(&FixtureConfig {
            project_count: 2,
            session_count: 3,
            orphan_event_count: 2,
            blob_size_per_part: 256,
            ..FixtureConfig::default()
        })
        .unwrap();

        let human = run(&fixture, &[]);
        assert!(
            human.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&human.stderr)
        );
        let human = stdout(&human);
        for heading in [
            "Database File Space",
            "Table and Index Space",
            "Project Attribution",
            "Largest Sessions",
            "Orphan Census",
            "Age Distribution",
            "External Directories",
        ] {
            assert!(
                human.contains(heading),
                "missing heading {heading}: {human}"
            );
        }

        let json = run(&fixture, &["--json"]);
        assert!(
            json.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&json.stderr)
        );
        let json: Value =
            serde_json::from_slice(&json.stdout).expect("stdout should be one JSON object");
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["mode"], "full");
        for key in [
            "file_space",
            "table_space",
            "project_attribution",
            "largest_sessions",
            "orphans",
            "age_distribution",
            "external_directories",
        ] {
            assert!(
                !json[key].is_null(),
                "full report layer {key} should be present"
            );
        }

        let page_count = json["file_space"]["page_count"].as_u64().unwrap();
        let project_count = json["project_attribution"].as_array().unwrap().len();
        let orphan_events = json["orphans"]["orphan_events"]["count"].as_u64().unwrap();
        // Column padding is a rendering detail; the contract is that each labelled fact
        // carries the same value the JSON report does.
        for (label, value) in [
            ("Page count", page_count.to_string()),
            ("Projects", project_count.to_string()),
            ("Orphan events", orphan_events.to_string()),
        ] {
            let line = human
                .lines()
                .find(|line| line.trim_start().starts_with(label))
                .unwrap_or_else(|| panic!("human report should carry a `{label}` line"));
            assert!(
                line.split_whitespace().any(|field| field == value),
                "`{label}` line `{line}` should carry `{value}`"
            );
        }
    }

    #[test]
    fn piped_human_output_has_no_ansi_escapes() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let output = run(&fixture, &[]);
        assert!(output.status.success());
        assert!(!output.stdout.contains(&0x1b));
    }

    #[test]
    fn json_stdout_contains_only_the_report_object() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let output = run(&fixture, &["--json"]);
        assert!(output.status.success());
        assert!(!output.stdout.contains(&0x1b));
        assert!(!stdout(&output).contains("INFO"));
        serde_json::from_slice::<Value>(&output.stdout).expect("stdout should contain JSON only");
    }

    #[test]
    fn top_limits_largest_sessions() {
        let fixture = Fixture::build(&FixtureConfig {
            session_count: 4,
            ..FixtureConfig::default()
        })
        .unwrap();
        let output = run(&fixture, &["--json", "--top", "2"]);
        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["largest_sessions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn json_logging_changes_stderr_without_touching_report_json() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let output = run(&fixture, &["--json", "--log", "json"]);
        assert!(output.status.success());
        serde_json::from_slice::<Value>(&output.stdout).expect("report should remain JSON");
        let diagnostics = String::from_utf8(output.stderr).unwrap();
        assert_ne!(diagnostics.trim(), "");
        for line in diagnostics.lines().filter(|line| !line.trim().is_empty()) {
            serde_json::from_str::<Value>(line).expect("each diagnostic should be JSON");
        }
    }

    #[test]
    fn json_logging_keeps_the_default_human_report() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let output = run(&fixture, &["--log", "json"]);
        assert!(output.status.success());
        assert!(stdout(&output).contains("Database File Space"));
        for line in String::from_utf8(output.stderr)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            serde_json::from_str::<Value>(line).expect("each diagnostic should be JSON");
        }
    }

    #[test]
    fn diagnostics_stay_silent_unless_logging_is_requested() {
        let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
        let output = run(&fixture, &[]);
        assert!(output.status.success());
        assert!(stdout(&output).contains("Database File Space"));
        assert_eq!(
            String::from_utf8(output.stderr).unwrap().trim(),
            "",
            "stderr must stay empty while --log is off"
        );
    }
}

mod analyze {
    pub mod command {
        use std::path::Path;
        use std::process::Command;

        use serde_json::Value;

        use crate::fixture::{Fixture, FixtureConfig};

        fn binary() -> Command {
            Command::new(env!("CARGO_BIN_EXE_oc-clean"))
        }

        #[test]
        fn quick_reports_file_accounting_and_row_counts_only() {
            let fixture = Fixture::build(&FixtureConfig {
                project_count: 2,
                session_count: 3,
                ..FixtureConfig::default()
            })
            .unwrap();
            let output = binary()
                .args(["analyze", "--db"])
                .arg(&fixture.database_path)
                .args(["--quick", "--json"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let json: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(json["mode"], "quick");
            assert_eq!(json["row_counts"]["project"], 2);
            assert_eq!(json["row_counts"]["session"], 3);
            for key in [
                "table_space",
                "project_attribution",
                "largest_sessions",
                "orphans",
                "age_distribution",
                "external_directories",
            ] {
                assert!(json[key].is_null(), "quick report should skip {key}");
            }
        }

        #[test]
        fn missing_database_uses_not_found_exit_contract() {
            let output = binary()
                .args([
                    "analyze",
                    "--db",
                    "/definitely/missing/oc-clean.db",
                    "--json",
                ])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(3));
            assert_eq!(output.stdout, Vec::<u8>::new());
            assert!(String::from_utf8_lossy(&output.stderr).contains("does not exist"));
        }

        #[test]
        fn analyze_fixture_does_not_modify_database() {
            let fixture = Fixture::build(&FixtureConfig::default()).unwrap();
            let before = metadata(&fixture.database_path);
            let output = binary()
                .args(["analyze", "--db"])
                .arg(&fixture.database_path)
                .arg("--json")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(metadata(&fixture.database_path), before);
        }

        fn metadata(path: &Path) -> (u64, std::time::SystemTime) {
            let metadata = path.metadata().unwrap();
            (metadata.len(), metadata.modified().unwrap())
        }
    }
}
