use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
#[cfg(unix)]
use rustix::fd::{AsFd, OwnedFd};
#[cfg(unix)]
use rustix::fs::{AtFlags, Dir, Mode, OFlags, openat, unlinkat};

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

/// Successful compaction metrics for one retained snapshot repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcRepositoryReport {
    pub path: PathBuf,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub reclaimed_bytes: u64,
}

/// One retained snapshot repository whose Git compaction was attempted and failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcRepositoryFailure {
    pub path: PathBuf,
    pub exit_code: Option<i32>,
    pub message: String,
}

/// Results from compacting retained snapshot repositories.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    pub compacted_repositories: Vec<GcRepositoryReport>,
    pub repository_failures: Vec<GcRepositoryFailure>,
    pub skipped_deleting_project_directories: u64,
    pub skipped_entries: u64,
}

impl GcReport {
    /// Converts attempted repository failures into the stable partial-success error category.
    #[must_use]
    pub fn partial_success_error(&self) -> Option<Error> {
        if self.repository_failures.is_empty() {
            return None;
        }
        let paths = self
            .repository_failures
            .iter()
            .map(|failure| failure.path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Some(Error::PartialSuccess {
            left_behind: format!("snapshot repositories not compacted: {paths}"),
        })
    }
}

/// Outcome of an explicitly requested retained-snapshot compaction pass.
#[derive(Debug, Eq, PartialEq)]
pub enum GcSnapshotsOutcome {
    Completed(GcReport),
    SkippedGitUnavailable { warning: String },
}

/// Compacts repositories belonging to retained projects while structurally excluding projects
/// slated for deletion.
///
/// This operation is opt-in. Snapshot removal functions never invoke it automatically.
///
/// # Errors
///
/// Returns a typed error when a project identifier is unsafe, the snapshot tree cannot be
/// inspected or measured, or Git availability probing fails for a reason other than an absent
/// executable. An attempted failure for one repository is collected in [`GcReport`] and does not
/// stop later repositories.
pub fn gc_retained(
    snapshot_root: &Path,
    retained_project_ids: &ProjectIds,
    deleting_project_ids: &ProjectIds,
) -> Result<GcSnapshotsOutcome, Error> {
    gc_retained_with_path(
        snapshot_root,
        retained_project_ids,
        deleting_project_ids,
        None,
    )
}

pub(crate) fn gc_retained_with_path(
    snapshot_root: &Path,
    retained_project_ids: &ProjectIds,
    deleting_project_ids: &ProjectIds,
    path: Option<&OsStr>,
) -> Result<GcSnapshotsOutcome, Error> {
    let retained_paths = retained_project_ids
        .iter()
        .map(|project_id| {
            snapshot_project_path(snapshot_root, project_id)
                .map(|project_path| (project_id, project_path))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for project_id in deleting_project_ids {
        validate_project_id(project_id)?;
    }
    if !git_is_available(path)? {
        return Ok(GcSnapshotsOutcome::SkippedGitUnavailable {
            warning: "SKIP: git is unavailable; retained snapshot repositories were not compacted"
                .to_owned(),
        });
    }
    if !is_real_directory(snapshot_root)? {
        return Ok(GcSnapshotsOutcome::Completed(GcReport::default()));
    }

    let mut report = GcReport::default();
    for (project_id, project_path) in retained_paths {
        if deleting_project_ids.contains(project_id) {
            report.skipped_deleting_project_directories = report
                .skipped_deleting_project_directories
                .saturating_add(1);
            continue;
        }
        if !is_real_directory(&project_path)? {
            report.skipped_entries = report.skipped_entries.saturating_add(1);
            continue;
        }
        for entry in directory_entries(&project_path)? {
            if !entry_is_directory(&entry)? {
                report.skipped_entries = report.skipped_entries.saturating_add(1);
                continue;
            }
            compact_repository(&entry.path(), path, &mut report)?;
        }
    }
    Ok(GcSnapshotsOutcome::Completed(report))
}

fn git_is_available(path: Option<&OsStr>) -> Result<bool, Error> {
    let mut command = Command::new("git");
    command.arg("--version");
    set_command_path(&mut command, path);
    match command.output() {
        Ok(output) if output.status.success() => Ok(true),
        Ok(output) => Err(io_error(
            Path::new("git"),
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "git --version exited unsuccessfully: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ),
        )),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error(Path::new("git"), source)),
    }
}

fn compact_repository(
    repository: &Path,
    path: Option<&OsStr>,
    report: &mut GcReport,
) -> Result<(), Error> {
    let before_bytes = directory_bytes(repository)?;
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(repository)
        .args(["gc", "--prune=now"]);
    set_command_path(&mut command, path);
    match command.output() {
        Ok(output) if output.status.success() => {
            let after_bytes = directory_bytes(repository)?;
            report.compacted_repositories.push(GcRepositoryReport {
                path: repository.to_path_buf(),
                before_bytes,
                after_bytes,
                reclaimed_bytes: before_bytes.saturating_sub(after_bytes),
            });
        }
        Ok(output) => report.repository_failures.push(GcRepositoryFailure {
            path: repository.to_path_buf(),
            exit_code: output.status.code(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }),
        Err(source) => report.repository_failures.push(GcRepositoryFailure {
            path: repository.to_path_buf(),
            exit_code: None,
            message: source.to_string(),
        }),
    }
    Ok(())
}

fn set_command_path(command: &mut Command, path: Option<&OsStr>) {
    if let Some(path) = path {
        command.env("PATH", path);
    }
}

fn directory_bytes(path: &Path) -> Result<u64, Error> {
    let mut bytes = 0_u64;
    for entry in directory_entries(path)? {
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(&entry.path(), source))?;
        if file_type.is_dir() {
            bytes = bytes.saturating_add(directory_bytes(&entry.path())?);
        } else if file_type.is_file() {
            bytes = bytes.saturating_add(
                entry
                    .metadata()
                    .map_err(|source| io_error(&entry.path(), source))?
                    .len(),
            );
        }
    }
    Ok(bytes)
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
    let projects = pruned_project_ids
        .iter()
        .map(|project_id| {
            snapshot_project_path(snapshot_root, project_id).map(|path| (project_id.as_str(), path))
        })
        .collect::<Result<Vec<_>, _>>()?;

    #[cfg(unix)]
    let Some(snapshot_directory) = open_snapshot_root(snapshot_root)? else {
        return Ok(RemovalReport::default());
    };
    #[cfg(not(unix))]
    if !is_real_directory(snapshot_root)? {
        return Ok(RemovalReport::default());
    }

    let mut report = RemovalReport::default();
    for (project_id, path) in projects {
        #[cfg(unix)]
        remove_candidate(&snapshot_directory, project_id.as_ref(), &path, &mut report)?;
        #[cfg(not(unix))]
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
    #[cfg(unix)]
    let Some(snapshot_directory) = open_snapshot_root(snapshot_root)? else {
        return Ok(RemovalReport::default());
    };
    #[cfg(not(unix))]
    if !is_real_directory(snapshot_root)? {
        return Ok(RemovalReport::default());
    }

    let mut report = RemovalReport::default();
    #[cfg(unix)]
    let entries = directory_names(&snapshot_directory, snapshot_root)?;
    #[cfg(not(unix))]
    let entries = directory_entries(snapshot_root)?;
    for entry in entries {
        #[cfg(unix)]
        let (project_id, path) = {
            let project_id = entry
                .to_str()
                .ok_or_else(|| invalid_project_id("snapshot directory name is not valid UTF-8"))?
                .to_owned();
            let path = snapshot_project_path(snapshot_root, &project_id)?;
            (project_id, path)
        };
        #[cfg(not(unix))]
        let (project_id, path) = {
            if !entry_is_directory(&entry)? {
                report.skipped_entries = report.skipped_entries.saturating_add(1);
                continue;
            }
            let project_id = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid_project_id("snapshot directory name is not valid UTF-8"))?;
            let path = snapshot_project_path(snapshot_root, &project_id)?;
            (project_id, path)
        };
        #[cfg(unix)]
        let Some(project_directory) =
            open_candidate(&snapshot_directory, &entry, &path, &mut report)?
        else {
            continue;
        };
        if project_exists(database.connection(), &project_id)? {
            report.retained_live_project_directories =
                report.retained_live_project_directories.saturating_add(1);
            continue;
        }
        #[cfg(unix)]
        remove_open_candidate(
            &snapshot_directory,
            &entry,
            &project_directory,
            &path,
            &mut report,
        );
        #[cfg(not(unix))]
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

#[cfg(unix)]
fn open_snapshot_root(path: &Path) -> Result<Option<OwnedFd>, Error> {
    match openat(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => Ok(Some(directory)),
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            Ok(None)
        }
        Err(source) => Err(io_error(path, errno_to_io(source))),
    }
}

#[cfg(unix)]
fn directory_names(directory: &OwnedFd, path: &Path) -> Result<Vec<std::ffi::OsString>, Error> {
    use std::os::unix::ffi::OsStringExt;

    let mut stream =
        Dir::read_from(directory).map_err(|source| io_error(path, errno_to_io(source)))?;
    let mut names = Vec::new();
    for entry in &mut stream {
        let entry = entry.map_err(|source| io_error(path, errno_to_io(source)))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(std::ffi::OsString::from_vec(name.to_vec()));
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(unix)]
fn open_candidate(
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
    report: &mut RemovalReport,
) -> Result<Option<OwnedFd>, Error> {
    match open_directory_at(parent, name) {
        Ok(directory) => Ok(Some(directory)),
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            report.skipped_entries = report.skipped_entries.saturating_add(1);
            Ok(None)
        }
        Err(source) => Err(io_error(path, errno_to_io(source))),
    }
}

#[cfg(unix)]
fn remove_candidate(
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
    report: &mut RemovalReport,
) -> Result<(), Error> {
    let Some(directory) = open_candidate(parent, name, path, report)? else {
        return Ok(());
    };
    remove_open_candidate(parent, name, &directory, path, report);
    Ok(())
}

#[cfg(unix)]
fn remove_open_candidate(
    parent: &OwnedFd,
    name: &OsStr,
    directory: &OwnedFd,
    path: &Path,
    report: &mut RemovalReport,
) {
    match remove_directory_contents(directory, path)
        .and_then(|()| unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(errno_to_io))
    {
        Ok(()) => {
            report.removed_directories = report.removed_directories.saturating_add(1);
        }
        Err(source) => report.directory_errors.push(RemovalDirectoryError {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(unix)]
fn remove_directory_contents(directory: &OwnedFd, path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let mut stream = Dir::read_from(directory).map_err(errno_to_io)?;
    while let Some(entry) = stream.read() {
        let entry = entry.map_err(errno_to_io)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let child_path = path.join(OsStr::from_bytes(name.to_bytes()));
        #[cfg(test)]
        run_before_descend_hook(&child_path);
        match open_directory_at(directory, OsStr::from_bytes(name.to_bytes())) {
            Ok(child_directory) => {
                remove_directory_contents(&child_directory, &child_path)?;
                unlinkat(directory, name, AtFlags::REMOVEDIR).map_err(errno_to_io)?;
            }
            Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                unlinkat(directory, name, AtFlags::empty()).map_err(errno_to_io)?;
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(source) => return Err(errno_to_io(source)),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_directory_at<Fd: AsFd>(parent: Fd, name: &OsStr) -> rustix::io::Result<OwnedFd> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

#[cfg(unix)]
fn errno_to_io(source: rustix::io::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(source.raw_os_error())
}

fn is_real_directory(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error(path, source)),
    }
}

#[cfg(not(unix))]
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

#[cfg(not(unix))]
fn remove_directory_tree(path: &Path) -> std::io::Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "refusing to traverse a non-directory snapshot entry",
        ));
    }
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

#[cfg(all(test, unix))]
#[derive(Debug)]
struct BeforeDescendHook {
    target: PathBuf,
    replacement: PathBuf,
    symlink_target: PathBuf,
}

#[cfg(all(test, unix))]
static BEFORE_DESCEND_HOOK: std::sync::Mutex<Option<BeforeDescendHook>> =
    std::sync::Mutex::new(None);

#[cfg(all(test, unix))]
fn run_before_descend_hook(path: &Path) {
    let hook = {
        let mut slot = BEFORE_DESCEND_HOOK
            .lock()
            .expect("before-descend hook lock should remain available");
        if slot.as_ref().is_some_and(|hook| hook.target == path) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        fs::rename(path, &hook.replacement).expect("target directory should move before descent");
        std::os::unix::fs::symlink(&hook.symlink_target, path)
            .expect("replacement symlink should be created before descent");
    }
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
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    use rusqlite::Connection;
    use tempfile::TempDir;

    use crate::db::{ConnectionOptions, ReadOnlyConnection, open_read_only};
    use crate::delete::projects::ProjectIds;
    use crate::error::Error;
    use crate::paths::Target;

    use super::{
        GcSnapshotsOutcome, gc_retained, gc_retained_with_path, remove_orphaned, remove_pruned,
    };

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

    fn bare_repository_with_loose_objects(
        fixture: &Fixture,
        project_id: &str,
        repository_name: &str,
    ) -> PathBuf {
        let repository = fixture.snapshot_root.join(project_id).join(repository_name);
        fs::create_dir_all(
            repository
                .parent()
                .expect("repository should have a parent"),
        )
        .expect("project snapshot directory should be created");
        let status = Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(&repository)
            .status()
            .expect("git should be available for repository fixtures");
        assert!(status.success(), "bare repository should initialize");

        for index in 0..64_u32 {
            let object = fixture.directory.path().join(format!("object-{index}"));
            let payload = (0..16_384_u32)
                .map(|offset| ((index.wrapping_mul(31) + offset.wrapping_mul(17)) % 251) as u8)
                .collect::<Vec<_>>();
            fs::write(&object, payload).expect("loose object payload should be written");
            let status = Command::new("git")
                .arg("--git-dir")
                .arg(&repository)
                .args(["hash-object", "-w"])
                .arg(&object)
                .status()
                .expect("git hash-object should run");
            assert!(status.success(), "loose object should be created");
        }
        repository
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

    #[cfg(unix)]
    #[test]
    fn nested_symlink_swap_cannot_escape_snapshot_root() {
        let fixture = Fixture::new();
        let removed = fixture.snapshot_directory("project-removed");
        let nested = removed.join("worktree-hash");
        let moved = fixture.directory.path().join("displaced-worktree-hash");
        let outside = fixture.directory.path().join("outside-snapshot");
        fs::create_dir_all(&outside).expect("outside directory should be created");
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"keep").expect("outside sentinel should be written");
        *super::BEFORE_DESCEND_HOOK
            .lock()
            .expect("before-descend hook lock should remain available") =
            Some(super::BeforeDescendHook {
                target: nested,
                replacement: moved.clone(),
                symlink_target: outside,
            });

        let report = remove_pruned(&fixture.snapshot_root, &project_ids(&["project-removed"]))
            .expect("pruned snapshot removal should finish safely");

        assert!(sentinel.exists(), "deletion escaped the snapshot root");
        assert!(moved.join("HEAD").exists());
        assert_eq!(report.removed_directories, 1);
        assert!(report.directory_errors.is_empty());
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

    #[test]
    fn gc_compacts_retained_bare_repository_and_reports_reclaimed_bytes() {
        let fixture = Fixture::new();
        let repository =
            bare_repository_with_loose_objects(&fixture, "project-retained", "worktree-hash");

        let GcSnapshotsOutcome::Completed(report) = gc_retained(
            &fixture.snapshot_root,
            &project_ids(&["project-retained"]),
            &ProjectIds::new(),
        )
        .expect("snapshot gc should complete") else {
            panic!("git should be available");
        };

        assert_eq!(report.repository_failures, Vec::new());
        assert!(report.partial_success_error().is_none());
        assert_eq!(report.compacted_repositories.len(), 1);
        let compacted = &report.compacted_repositories[0];
        assert_eq!(compacted.path, repository);
        assert!(compacted.before_bytes > compacted.after_bytes);
        assert!(compacted.reclaimed_bytes > 0);
    }

    #[test]
    fn gc_reports_documented_skip_when_git_is_unavailable() {
        let fixture = Fixture::new();
        let _repository =
            bare_repository_with_loose_objects(&fixture, "project-retained", "worktree-hash");

        let outcome = gc_retained_with_path(
            &fixture.snapshot_root,
            &project_ids(&["project-retained"]),
            &ProjectIds::new(),
            Some(OsStr::new("")),
        )
        .expect("missing git should be a successful skip");

        let GcSnapshotsOutcome::SkippedGitUnavailable { warning } = outcome else {
            panic!("missing git should produce the documented skip outcome");
        };
        assert!(warning.contains("SKIP"));
    }

    #[test]
    fn gc_continues_after_corrupt_repository_and_reports_partial_success() {
        let fixture = Fixture::new();
        let corrupt = fixture
            .snapshot_root
            .join("project-retained")
            .join("a-corrupt");
        fs::create_dir_all(&corrupt).expect("corrupt repository directory should be created");
        fs::write(corrupt.join("sentinel"), b"not a git repository")
            .expect("corrupt repository marker should be written");
        let valid = bare_repository_with_loose_objects(&fixture, "project-retained", "z-valid");

        let GcSnapshotsOutcome::Completed(report) = gc_retained(
            &fixture.snapshot_root,
            &project_ids(&["project-retained"]),
            &ProjectIds::new(),
        )
        .expect("one failed repository should not abort snapshot gc") else {
            panic!("git should be available");
        };

        assert_eq!(report.compacted_repositories.len(), 1);
        assert_eq!(report.compacted_repositories[0].path, valid);
        assert_eq!(report.repository_failures.len(), 1);
        assert_eq!(report.repository_failures[0].path, corrupt);
        assert!(
            !report
                .compacted_repositories
                .iter()
                .any(|compacted| compacted.path == corrupt)
        );
        let partial = report
            .partial_success_error()
            .expect("an attempted gc failure should map to partial success");
        assert_eq!(partial.exit_code(), 10);
    }

    #[test]
    fn gc_never_runs_for_project_slated_for_deletion() {
        let fixture = Fixture::new();
        let doomed = fixture.snapshot_root.join("project-doomed").join("corrupt");
        fs::create_dir_all(&doomed).expect("doomed repository directory should be created");
        fs::write(doomed.join("sentinel"), b"not a git repository")
            .expect("doomed repository marker should be written");
        let retained =
            bare_repository_with_loose_objects(&fixture, "project-retained", "worktree-hash");

        let GcSnapshotsOutcome::Completed(report) = gc_retained(
            &fixture.snapshot_root,
            &project_ids(&["project-doomed", "project-retained"]),
            &project_ids(&["project-doomed"]),
        )
        .expect("snapshot gc should complete") else {
            panic!("git should be available");
        };

        assert_eq!(report.skipped_deleting_project_directories, 1);
        assert_eq!(report.compacted_repositories.len(), 1);
        assert_eq!(report.compacted_repositories[0].path, retained);
        assert_eq!(report.repository_failures, Vec::new());
        assert!(doomed.join("sentinel").exists());
    }
}
