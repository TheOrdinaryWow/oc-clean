//! End-to-end coverage for terminal presentation: session descriptions, the deletion
//! preview, failure rendering, and the log switch.

use std::process::Stdio;

use super::fixture::{Fixture, FixtureConfig};
use super::support::{assert_code, binary, command, json};

fn fixture() -> Fixture {
    Fixture::build(&FixtureConfig {
        session_count: 6,
        // Archived sessions give the `--archived` selector something to preview.
        archived_session_count: 6,
        messages_per_session: 2,
        parts_per_message: 2,
        blob_size_per_part: 1_024,
        ..FixtureConfig::default()
    })
    .expect("fixture should build")
}

#[test]
fn analyze_describes_each_reported_session_in_both_output_forms() {
    let fixture = fixture();

    let human = command(&fixture, "analyze")
        .output()
        .expect("human analyze should run");
    let machine = command(&fixture, "analyze")
        .arg("--json")
        .output()
        .expect("JSON analyze should run");

    assert_code(&human, 0);
    assert_code(&machine, 0);

    let rendered = String::from_utf8(human.stdout).expect("human report should be UTF-8");
    for header in ["Session", "Title", "Msgs", "Last active"] {
        assert!(
            rendered.contains(header),
            "the largest-session table should carry a `{header}` column"
        );
    }

    let sessions = json(&machine)["largest_sessions"]
        .as_array()
        .expect("largest_sessions should be an array")
        .clone();
    assert_ne!(sessions.len(), 0, "the fixture has sessions to report");
    for session in &sessions {
        assert!(
            session["title"].is_string(),
            "title should be additive JSON"
        );
        assert!(session["time_updated"].is_i64());
        assert!(session["message_count"].is_u64());
        let title = session["title"].as_str().expect("title is a string");
        assert!(
            rendered.contains(title) || rendered.contains(&title[..title.len().min(8)]),
            "the human report should name the session the JSON report names"
        );
    }
}

#[test]
fn a_clean_dry_run_previews_the_sessions_it_would_delete() {
    let fixture = fixture();

    let human = command(&fixture, "clean")
        .args(["--archived", "--keep-recent", "0", "--top", "3"])
        .output()
        .expect("human dry run should run");
    let machine = command(&fixture, "clean")
        .args(["--archived", "--keep-recent", "0", "--top", "3", "--json"])
        .output()
        .expect("JSON dry run should run");

    assert_code(&human, 0);
    assert_code(&machine, 0);

    let rendered = String::from_utf8(human.stdout).expect("dry run should be UTF-8");
    assert!(rendered.contains("Cleanup Impact (dry-run)"));
    assert!(rendered.contains("Largest Selected Sessions"));

    let preview = json(&machine)["impact"]["preview"]
        .as_array()
        .expect("preview should be an array")
        .clone();
    assert!(
        preview.len() <= 3,
        "--top caps the preview at {} entries, got {}",
        3,
        preview.len()
    );
    for session in &preview {
        assert!(session["session_id"].is_string());
        assert!(session["title"].is_string());
        assert!(session["message_count"].is_u64());
    }
}

#[test]
fn the_preview_never_exceeds_the_requested_cap() {
    let fixture = fixture();

    let output = command(&fixture, "clean")
        .args(["--archived", "--keep-recent", "0", "--top", "1", "--json"])
        .output()
        .expect("dry run should run");

    assert_code(&output, 0);
    assert_eq!(
        json(&output)["impact"]["preview"]
            .as_array()
            .expect("preview should be an array")
            .len(),
        1
    );
}

#[test]
fn a_failure_is_visible_on_stderr_while_logging_stays_off() {
    let output = binary()
        .args(["analyze", "--db", "/tmp/oc-clean-e2e-absent.db"])
        .output()
        .expect("analyze should run");

    assert_code(&output, 3);
    let diagnostics = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(
        diagnostics.contains("error:"),
        "a failure must be reported even with diagnostics off: {diagnostics}"
    );
    assert!(diagnostics.contains("/tmp/oc-clean-e2e-absent.db"));
    assert!(
        !diagnostics.contains("INFO"),
        "tracing diagnostics stay off unless --log is supplied"
    );
    assert!(
        output.stdout.is_empty(),
        "a failure must not write to the report stream"
    );
}

#[test]
fn a_json_failure_is_one_parsable_object_carrying_the_stable_kind() {
    let output = binary()
        .args(["analyze", "--json", "--db", "/tmp/oc-clean-e2e-absent.db"])
        .output()
        .expect("analyze should run");

    assert_code(&output, 3);
    let diagnostics = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let value: serde_json::Value =
        serde_json::from_str(diagnostics.trim()).expect("the failure should parse as JSON");

    assert_eq!(value["kind"], "not_found");
    assert_eq!(value["exit_code"], 3);
    assert!(
        output.stdout.is_empty(),
        "stdout must keep carrying exactly one report object, or none"
    );
}

#[test]
fn a_failure_hint_names_the_next_step_when_one_exists() {
    let fixture = fixture();
    // A missing Tier 1 column is a schema failure, which carries a documented next step.
    fixture
        .connect()
        .expect("fixture should connect")
        .execute_batch("ALTER TABLE session DROP COLUMN time_archived")
        .expect("dropping a Tier 1 column should succeed");

    let output = command(&fixture, "analyze")
        .output()
        .expect("analyze should run");

    assert_code(&output, 4);
    let diagnostics = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(diagnostics.contains("error:"));
    assert!(diagnostics.contains("hint:"));
    assert!(diagnostics.contains("--force-schema"));
}

#[test]
fn progress_is_never_drawn_into_a_redirected_stream() {
    let fixture = fixture();

    let output = command(&fixture, "analyze")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("analyze should run");

    assert_code(&output, 0);
    assert!(
        !output.stderr.contains(&0x1b),
        "a redirected stderr must not receive progress redraw sequences"
    );
    assert!(
        !output.stdout.contains(&0x1b),
        "a redirected stdout must not receive ANSI styling"
    );
}
