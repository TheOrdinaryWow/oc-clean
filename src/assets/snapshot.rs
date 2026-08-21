use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::db::DatabaseConnection;
use crate::delete::projects::ProjectIds;
use crate::error::Error;

/// One snapshot directory that could not be removed.
#[derive(Debug)]
pub struct RemovalDirectoryError {
    pub path: PathBuf,
    pub source: std::io::Error,
}

/// Counts produced by snapshot directory removal.
#[derive(Debug, Default)]
pub struct RemovalReport {
    pub removed_directories: u64,
    pub retained_live_project_directories: u64,
    pub skipped_entries: u64,
    pub directory_errors: Vec<RemovalDirectoryError>,
}

/// Removes snapshot directories for projects pruned by the current clean operation.
///
/// # Errors
///
/// Returns a typed error when a project identifier is unsafe or an existing snapshot root cannot
/// be inspected.
pub fn remove_pruned(
    snapshot_root: &Path,
    pruned_project_ids: &ProjectIds,
) -> Result<RemovalReport, Error> {
    let project_paths = pruned_project_ids
        .iter()
        .map(|project_id| snapshot_project_path(snapshot_root, project_id))
        .collect::<Result<Vec<_>, _>>()?;
    if !is_real_directory(snapshot_root)? {
        return Ok(RemovalReport::default());
    }

    let mut report = RemovalReport::default();
    for path in project_paths {
        remove_candidate(&path, &mut report)?;
    }
    Ok(report)
}

/// Removes snapshot directories whose projects are absent from the database.
///
/// # Errors
///
/// Returns a typed error when SQLite cannot check project liveness, a snapshot directory name is
/// unsafe, or an existing snapshot root cannot be enumerated.
pub fn remove_orphaned<Access>(
    database: &DatabaseConnection<Access>,
    snapshot_root: &Path,
) -> Result<RemovalReport, Error> {
    if !is_real_directory(snapshot_root)? {
        return Ok(RemovalReport::default());
    }

    let mut report = RemovalReport::default();
    for entry in directory_entries(snapshot_root)? {
        if !entry_is_directory(&entry)? {
            report.skipped_entries = report.skipped_entries.saturating_add(1);
            continue;
        }
        let project_id = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid_project_id("snapshot directory name is not valid UTF-8"))?;
        let path = snapshot_project_path(snapshot_root, &project_id)?;
        if project_exists(database.connection(), &project_id)? {
            report.retained_live_project_directories =
                report.retained_live_project_directories.saturating_add(1);
            continue;
        }
        remove_candidate(&path, &mut report)?;
    }
    Ok(report)
}

fn snapshot_project_path(snapshot_root: &Path, project_id: &str) -> Result<PathBuf, Error> {
    validate_project_id(project_id)?;
    let path = snapshot_root.join(project_id);
    if path.parent() != Some(snapshot_root) {
        return Err(invalid_project_id(project_id));
    }
    Ok(path)
}

fn validate_project_id(project_id: &str) -> Result<(), Error> {
    if project_id.is_empty()
        || project_id.contains("..")
        || project_id.contains('/')
        || project_id.contains('\\')
    {
        return Err(invalid_project_id(project_id));
    }
    Ok(())
}

fn invalid_project_id(project_id: &str) -> Error {
    Error::InvalidArgument {
        argument: "project.id".to_owned(),
        reason: format!("unsafe snapshot project identifier `{project_id}`"),
    }
}

fn is_real_directory(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error(path, source)),
    }
}

fn remove_candidate(path: &Path, report: &mut RemovalReport) -> Result<(), Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            report.skipped_entries = report.skipped_entries.saturating_add(1);
            return Ok(());
        }
        Err(source) => return Err(io_error(path, source)),
    };
    if !metadata.file_type().is_dir() {
        report.skipped_entries = report.skipped_entries.saturating_add(1);
        return Ok(());
    }
    match remove_directory_tree(path) {
        Ok(()) => {
            report.removed_directories = report.removed_directories.saturating_add(1);
        }
        Err(source) => report.directory_errors.push(RemovalDirectoryError {
            path: path.to_path_buf(),
            source,
        }),
    }
    Ok(())
}

fn remove_directory_tree(path: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        if entry.file_type()?.is_dir() {
            remove_directory_tree(&child)?;
        } else {
            fs::remove_file(child)?;
        }
    }
    fs::remove_dir(path)
}

fn project_exists(connection: &Connection, project_id: &str) -> Result<bool, Error> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM project WHERE id = ?1)",
            [project_id],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error("re-validating a snapshot directory project", source))
}

fn directory_entries(path: &Path) -> Result<Vec<fs::DirEntry>, Error> {
    let read_dir = match fs::read_dir(path) {
        Ok(read_dir) => read_dir,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(path, source)),
    };
    let mut entries = read_dir
        .map(|entry| entry.map_err(|source| io_error(path, source)))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn entry_is_directory(entry: &fs::DirEntry) -> Result<bool, Error> {
    entry
        .file_type()
        .map(|file_type| file_type.is_dir())
        .map_err(|source| io_error(&entry.path(), source))
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        context: context.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use rusqlite::Connection;
    use tempfile::TempDir;

    use crate::db::{ConnectionOptions, ReadOnlyConnection, open_read_only};
    use crate::delete::projects::ProjectIds;
    use crate::error::Error;
    use crate::paths::Target;

    use super::{remove_orphaned, remove_pruned};

    struct Fixture {
        directory: TempDir,
        database_path: PathBuf,
        snapshot_root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("temporary directory should be created");
            let database_path = directory.path().join("opencode.db");
            Connection::open(&database_path)
                .expect("fixture database should open")
                .execute_batch("CREATE TABLE project (id TEXT PRIMARY KEY NOT NULL);")
                .expect("project table should be created");
            Self {
                snapshot_root: directory.path().join("snapshot"),
                directory,
                database_path,
            }
        }

        fn database(&self) -> ReadOnlyConnection {
            open_read_only(
                &Target::File(self.database_path.clone()),
                ConnectionOptions::default(),
            )
            .expect("fixture should open read-only")
        }

        fn insert_project(&self, project_id: &str) {
            Connection::open(&self.database_path)
                .expect("fixture database should reopen")
                .execute("INSERT INTO project (id) VALUES (?1)", [project_id])
                .expect("project should insert");
        }

        fn snapshot_directory(&self, project_id: &str) -> PathBuf {
            let directory = self.snapshot_root.join(project_id).join("worktree-hash");
            fs::create_dir_all(&directory).expect("snapshot directory should be created");
            fs::write(directory.join("HEAD"), b"ref: refs/heads/main\n")
                .expect("snapshot file should be written");
            self.snapshot_root.join(project_id)
        }
    }

    fn project_ids(values: &[&str]) -> ProjectIds {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn removes_exactly_the_pruned_project_snapshot_directory() {
        let fixture = Fixture::new();
        let removed = fixture.snapshot_directory("project-removed");
        let retained_a = fixture.snapshot_directory("project-retained-a");
        let retained_b = fixture.snapshot_directory("project-retained-b");

        let report = remove_pruned(&fixture.snapshot_root, &project_ids(&["project-removed"]))
            .expect("pruned snapshot removal should succeed");

        assert_eq!(report.removed_directories, 1);
        assert_eq!(report.skipped_entries, 0);
        assert!(report.directory_errors.is_empty());
        assert!(!removed.exists());
        assert!(retained_a.join("worktree-hash/HEAD").exists());
        assert!(retained_b.join("worktree-hash/HEAD").exists());
    }

    #[test]
    fn orphan_scan_removes_only_absent_projects_and_retains_live_projects() {
        let fixture = Fixture::new();
        fixture.insert_project("project-live");
        let live = fixture.snapshot_directory("project-live");
        let orphaned = fixture.snapshot_directory("project-orphaned");

        let report = remove_orphaned(&fixture.database(), &fixture.snapshot_root)
            .expect("orphan scan should succeed");

        assert_eq!(report.removed_directories, 1);
        assert_eq!(report.retained_live_project_directories, 1);
        assert!(report.directory_errors.is_empty());
        assert!(live.join("worktree-hash/HEAD").exists());
        assert!(!orphaned.exists());
    }

    #[test]
    fn rejects_unsafe_project_id_before_touching_the_filesystem() {
        let fixture = Fixture::new();
        let retained = fixture.snapshot_directory("project-retained");
        let outside = fixture.directory.path().join("outside");
        fs::create_dir_all(&outside).expect("outside directory should be created");
        fs::write(outside.join("sentinel"), b"keep").expect("outside file should be written");

        let error = remove_pruned(
            &fixture.snapshot_root,
            &project_ids(&["project-retained", "../../outside"]),
        )
        .expect_err("unsafe project id should be rejected");

        assert!(matches!(error, Error::InvalidArgument { .. }));
        assert!(retained.join("worktree-hash/HEAD").exists());
        assert!(outside.join("sentinel").exists());
    }

    #[cfg(unix)]
    #[test]
    fn orphan_scan_does_not_follow_symlinked_directories() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = fixture.directory.path().join("outside-snapshot");
        fs::create_dir_all(&outside).expect("outside directory should be created");
        fs::write(outside.join("sentinel"), b"keep").expect("outside file should be written");
        fs::create_dir_all(&fixture.snapshot_root).expect("snapshot root should be created");
        let linked = fixture.snapshot_root.join("project-linked");
        symlink(&outside, &linked).expect("snapshot symlink should be created");

        let report = remove_orphaned(&fixture.database(), &fixture.snapshot_root)
            .expect("symlink-safe orphan scan should succeed");

        assert_eq!(report.removed_directories, 0);
        assert_eq!(report.skipped_entries, 1);
        assert!(report.directory_errors.is_empty());
        assert!(linked.symlink_metadata().is_ok());
        assert!(outside.join("sentinel").exists());
    }
}
