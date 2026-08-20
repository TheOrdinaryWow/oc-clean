use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{
    data_dir, database_target, derived_paths, DatabaseOptions, DerivedPaths, Environment, Platform,
    Target,
};
use crate::error::Error;

fn environment(entries: &[(&str, &str)]) -> Environment {
    entries
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

fn options<'input>(
    platform: Platform,
    explicit: Option<&'input Path>,
    channel: Option<&'input str>,
) -> DatabaseOptions<'input> {
    DatabaseOptions {
        explicit,
        channel,
        platform,
    }
}

#[test]
fn uses_non_empty_xdg_data_home_on_every_platform() {
    let environment = environment(&[("XDG_DATA_HOME", "/tmp/x"), ("HOME", "/ignored")]);

    for platform in [Platform::Linux, Platform::MacOs, Platform::Windows] {
        let actual = data_dir(&environment, platform).expect("XDG data directory should resolve");

        assert_eq!(actual, PathBuf::from("/tmp/x/opencode"));
    }
}

#[test]
fn uses_xdg_style_home_fallback_on_unix_platforms() {
    let cases = [
        (Platform::Linux, "/home/alice"),
        (Platform::MacOs, "/Users/alice"),
    ];

    for (platform, home) in cases {
        let environment = environment(&[("HOME", home)]);

        let actual = data_dir(&environment, platform).expect("Unix home should resolve");

        assert_eq!(actual, PathBuf::from(home).join(".local/share/opencode"));
        assert!(!actual
            .to_string_lossy()
            .contains("Library/Application Support"));
    }
}

#[test]
fn uses_userprofile_on_windows_when_home_is_unset() {
    let environment = environment(&[
        ("USERPROFILE", r"C:\Users\Alice"),
        ("LOCALAPPDATA", r"C:\Users\Alice\AppData\Local"),
    ]);

    let actual = data_dir(&environment, Platform::Windows).expect("USERPROFILE should resolve");

    assert_eq!(
        actual,
        PathBuf::from(r"C:\Users\Alice").join(".local/share/opencode")
    );
    assert!(!actual.to_string_lossy().contains("AppData"));
}

#[test]
fn treats_empty_xdg_data_home_as_unset() {
    let environment = environment(&[("XDG_DATA_HOME", ""), ("HOME", "/home/alice")]);

    let actual = data_dir(&environment, Platform::Linux).expect("HOME should resolve");

    assert_eq!(actual, PathBuf::from("/home/alice/.local/share/opencode"));
}

#[test]
fn returns_typed_error_when_required_home_is_missing() {
    let environment = Environment::new();

    let actual = data_dir(&environment, Platform::Linux);

    assert!(matches!(
        actual,
        Err(Error::InvalidArgument { argument, reason })
            if argument == "HOME" && reason.contains("XDG_DATA_HOME")
    ));
}

#[test]
fn explicit_database_path_overrides_environment_and_channel() {
    let environment = environment(&[
        ("XDG_DATA_HOME", "/tmp/x"),
        ("OPENCODE_DB", ":memory:"),
        ("OPENCODE_DISABLE_CHANNEL_DB", "true"),
    ]);
    let explicit = Path::new("/var/lib/custom.db");

    let actual = database_target(
        &environment,
        options(Platform::Linux, Some(explicit), Some("nightly")),
    )
    .expect("explicit path should resolve");

    assert_eq!(actual, Target::File(explicit.to_path_buf()));
}

#[test]
fn explicit_memory_database_is_a_non_file_target() {
    let environment = Environment::new();

    let actual = database_target(
        &environment,
        options(Platform::Linux, Some(Path::new(":memory:")), None),
    )
    .expect("explicit memory target should resolve");

    assert_eq!(actual, Target::Memory);
}

#[test]
fn resolves_opencode_db_absolute_relative_and_memory_values() {
    let cases = [
        (
            "/var/lib/opencode.db",
            Target::File(PathBuf::from("/var/lib/opencode.db")),
        ),
        (
            "custom.db",
            Target::File(PathBuf::from("/tmp/x/opencode/custom.db")),
        ),
        (":memory:", Target::Memory),
    ];

    for (value, expected) in cases {
        let environment = environment(&[("XDG_DATA_HOME", "/tmp/x"), ("OPENCODE_DB", value)]);

        let actual = database_target(&environment, options(Platform::Linux, None, None))
            .expect("OPENCODE_DB should resolve");

        assert_eq!(actual, expected);
    }
}

#[test]
fn preserves_windows_absolute_opencode_db_path() {
    let environment = environment(&[
        ("USERPROFILE", r"C:\Users\Alice"),
        ("OPENCODE_DB", r"D:\OpenCode\custom.db"),
    ]);

    let actual = database_target(&environment, options(Platform::Windows, None, None))
        .expect("absolute OPENCODE_DB should resolve");

    assert_eq!(
        actual,
        Target::File(PathBuf::from(r"D:\OpenCode\custom.db"))
    );
}

#[test]
fn uses_default_database_for_latest_beta_and_prod_channels() {
    for channel in [None, Some("latest"), Some("beta"), Some("prod")] {
        let environment = environment(&[("XDG_DATA_HOME", "/tmp/x")]);

        let actual = database_target(&environment, options(Platform::Linux, None, channel))
            .expect("standard channel should resolve");

        assert_eq!(
            actual,
            Target::File(PathBuf::from("/tmp/x/opencode/opencode.db"))
        );
    }
}

#[test]
fn sanitizes_custom_channel_for_database_filename() {
    let environment = environment(&[("XDG_DATA_HOME", "/tmp/x")]);

    let actual = database_target(
        &environment,
        options(Platform::Linux, None, Some("nightly/foo 🚀")),
    )
    .expect("custom channel should resolve");

    assert_eq!(
        actual,
        Target::File(PathBuf::from("/tmp/x/opencode/opencode-nightly-foo--.db"))
    );
}

#[test]
fn channel_database_disable_accepts_only_one_and_true() {
    let cases = [
        (Some("1"), "opencode.db"),
        (Some("true"), "opencode.db"),
        (Some("false"), "opencode-nightly.db"),
        (None, "opencode-nightly.db"),
    ];

    for (value, expected_name) in cases {
        let mut environment = environment(&[("XDG_DATA_HOME", "/tmp/x")]);
        if let Some(value) = value {
            environment.insert(
                OsString::from("OPENCODE_DISABLE_CHANNEL_DB"),
                OsString::from(value),
            );
        }

        let actual = database_target(
            &environment,
            options(Platform::Linux, None, Some("nightly")),
        )
        .expect("channel setting should resolve");

        assert_eq!(
            actual,
            Target::File(PathBuf::from("/tmp/x/opencode").join(expected_name))
        );
    }
}

#[test]
fn exposes_all_derived_data_directories() {
    let root = Path::new("/tmp/x/opencode");

    let actual = derived_paths(root);

    assert_eq!(
        actual,
        DerivedPaths {
            storage: root.join("storage"),
            snapshot: root.join("snapshot"),
            tool_output: root.join("tool-output"),
            log: root.join("log"),
        }
    );
}
