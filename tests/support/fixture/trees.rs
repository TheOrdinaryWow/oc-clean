use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{FixtureConfig, FixtureResult};

pub(super) fn create_storage(
    root: &Path,
    session_ids: &[String],
    config: &FixtureConfig,
) -> FixtureResult<Vec<PathBuf>> {
    let bucket = root.join("session");
    fs::create_dir_all(&bucket)?;
    for id in session_ids {
        fs::write(bucket.join(format!("{id}.json")), b"{}")?;
    }
    let mut orphans = Vec::with_capacity(config.orphan_storage_file_count);
    for index in 0..config.orphan_storage_file_count {
        let path = bucket.join(format!("ses_orphan{index}.json"));
        fs::write(&path, b"{}")?;
        orphans.push(path);
    }
    Ok(orphans)
}

pub(super) fn create_snapshots(root: &Path, config: &FixtureConfig) -> FixtureResult<Vec<PathBuf>> {
    fs::create_dir_all(root)?;
    for index in 0..config.project_count {
        create_bare_git_dir(&root.join(format!("project-{index}/hash-{index}")))?;
    }
    let mut orphans = Vec::with_capacity(config.orphan_snapshot_dir_count);
    for index in 0..config.orphan_snapshot_dir_count {
        let path = root.join(format!("project-orphan-{index}/hash-orphan-{index}"));
        create_bare_git_dir(&path)?;
        orphans.push(path);
    }
    Ok(orphans)
}

fn create_bare_git_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path.join("objects"))?;
    fs::create_dir_all(path.join("refs"))?;
    fs::write(path.join("HEAD"), b"ref: refs/heads/main\n")
}
