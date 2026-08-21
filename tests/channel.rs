use std::path::Path;
use std::process::Command;

const SUBCOMMANDS: &[&str] = &["analyze", "doctor", "clean", "vacuum"];

#[test]
fn global_channel_flag_targets_channel_suffixed_database_for_every_command() {
    for subcommand in SUBCOMMANDS {
        let data_home = tempfile::tempdir().expect("temporary data home should exist");
        let output = binary(data_home.path())
            .arg(subcommand)
            .args(["--channel", "nightly"])
            .output()
            .expect("oc-clean should run");

        assert_channel_target(subcommand, &output.stderr, "opencode-nightly.db");
    }
}

#[test]
fn channel_environment_targets_channel_suffixed_database_for_every_command() {
    for subcommand in SUBCOMMANDS {
        let data_home = tempfile::tempdir().expect("temporary data home should exist");
        let output = binary(data_home.path())
            .arg(subcommand)
            .env("OCC_CHANNEL", "preview")
            .output()
            .expect("oc-clean should run");

        assert_channel_target(subcommand, &output.stderr, "opencode-preview.db");
    }
}

fn binary(data_home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oc-clean"));
    command
        .env("XDG_DATA_HOME", data_home)
        .env_remove("OCC_CHANNEL")
        .env_remove("OCC_DB")
        .env_remove("OPENCODE_DB");
    command
}

fn assert_channel_target(subcommand: &str, stderr: &[u8], expected_filename: &str) {
    let stderr = String::from_utf8_lossy(stderr);
    assert!(
        stderr.contains(expected_filename),
        "{subcommand} should resolve {expected_filename}; stderr: {stderr}"
    );
}
