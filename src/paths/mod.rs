use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::error::Error;

pub type Environment = BTreeMap<OsString, OsString>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    Linux,
    MacOs,
    Windows,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    File(PathBuf),
    Memory,
}

#[derive(Clone, Copy, Debug)]
pub struct DatabaseOptions<'input> {
    pub explicit: Option<&'input Path>,
    pub channel: Option<&'input str>,
    pub platform: Platform,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedPaths {
    pub storage: PathBuf,
    pub snapshot: PathBuf,
    pub tool_output: PathBuf,
    pub log: PathBuf,
}

/// Resolves `OpenCode`'s data directory from an injected environment.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when both `XDG_DATA_HOME` and the platform's home variable
/// are unset or empty.
pub fn data_dir(environment: &Environment, platform: Platform) -> Result<PathBuf, Error> {
    if let Some(xdg_data_home) = non_empty(environment, "XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg_data_home).join("opencode"));
    }

    let home_variable = match platform {
        Platform::Linux | Platform::MacOs => "HOME",
        Platform::Windows => "USERPROFILE",
    };
    let home = non_empty(environment, home_variable).ok_or_else(|| Error::InvalidArgument {
        argument: home_variable.to_owned(),
        reason: "required to resolve the OpenCode data directory when XDG_DATA_HOME is unset"
            .to_owned(),
    })?;

    Ok(PathBuf::from(home).join(".local/share/opencode"))
}

/// Resolves the database target according to explicit, environment, and channel precedence.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when database resolution needs the data directory and its
/// required home variables are unset or empty.
pub fn database_target(
    environment: &Environment,
    options: DatabaseOptions<'_>,
) -> Result<Target, Error> {
    if let Some(explicit) = options.explicit {
        return Ok(target_from_path(explicit));
    }

    if let Some(configured) = non_empty(environment, "OPENCODE_DB") {
        let configured = Path::new(configured);
        if configured == Path::new(":memory:") {
            return Ok(Target::Memory);
        }
        if is_absolute(configured, options.platform) {
            return Ok(Target::File(configured.to_path_buf()));
        }

        return Ok(Target::File(
            data_dir(environment, options.platform)?.join(configured),
        ));
    }

    let channel = options.channel.unwrap_or("latest");
    let channel_database_disabled = matches!(
        environment
            .get(OsStr::new("OPENCODE_DISABLE_CHANNEL_DB"))
            .and_then(|value| value.to_str()),
        Some("1" | "true")
    );
    let filename = if channel_database_disabled || matches!(channel, "latest" | "beta" | "prod") {
        OsString::from("opencode.db")
    } else {
        OsString::from(format!("opencode-{}.db", sanitize_channel(channel)))
    };

    Ok(Target::File(
        data_dir(environment, options.platform)?.join(filename),
    ))
}

/// Placeholder data directory for an in-memory database, which owns no external storage.
///
/// It must stay a syntactically valid relative path. Deriving siblings from the SQLite keyword
/// `:memory:` yields `:memory:\\storage`, which Windows rejects as malformed rather than
/// reporting as absent, turning "no external storage" into a command failure.
pub const MEMORY_DATA_DIR: &str = "oc-clean-memory-target";

#[must_use]
pub fn derived_paths(data_dir: &Path) -> DerivedPaths {
    DerivedPaths {
        storage: data_dir.join("storage"),
        snapshot: data_dir.join("snapshot"),
        tool_output: data_dir.join("tool-output"),
        log: data_dir.join("log"),
    }
}

fn non_empty<'environment>(
    environment: &'environment Environment,
    name: &str,
) -> Option<&'environment OsStr> {
    environment
        .get(OsStr::new(name))
        .map(OsString::as_os_str)
        .filter(|value| !value.is_empty())
}

fn target_from_path(path: &Path) -> Target {
    if path == Path::new(":memory:") {
        Target::Memory
    } else {
        Target::File(path.to_path_buf())
    }
}

fn is_absolute(path: &Path, platform: Platform) -> bool {
    match platform {
        Platform::Linux | Platform::MacOs => path.is_absolute(),
        Platform::Windows => windows_path_is_absolute(path),
    }
}

fn windows_path_is_absolute(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return path.is_absolute();
    };
    let bytes = path.as_bytes();
    let has_drive_root = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    let has_root = bytes
        .first()
        .is_some_and(|byte| matches!(byte, b'/' | b'\\'));

    has_drive_root || has_root
}

fn sanitize_channel(channel: &str) -> String {
    channel
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
