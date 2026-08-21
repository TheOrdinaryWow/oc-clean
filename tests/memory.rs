use std::process::{Command, Output};

use serde_json::Value;

#[test]
fn analyze_accepts_fresh_in_memory_database() {
    let output = run("analyze");

    assert_success(&output);
    let report: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(report["mode"], "full");
    assert_eq!(report["row_counts"]["session"], 0);
}

#[test]
fn doctor_accepts_fresh_in_memory_database() {
    let output = run("doctor");

    assert_success(&output);
    let report: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(report["database"], ":memory:");
    assert_eq!(report["integrity_check"]["ok"], true);
    assert_eq!(report["timestamp_sanity"]["ok"], true);
}

fn run(subcommand: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oc-clean"))
        .args([subcommand, "--db", ":memory:", "--json"])
        .env_remove("NO_COLOR")
        .output()
        .expect("oc-clean should run")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
