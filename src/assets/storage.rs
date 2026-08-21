#[cfg(all(test, unix))]
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::db::{self, DatabaseConnection};
use crate::error::Error;
use crate::select::predicates::SessionIds;

use super::traversal::AnchoredDir;

/// Selects the conforming storage files eligible for an orphan sweep.
#[derive(Clone, Copy, Debug)]
pub enum SweepScope<'session_ids> {
    /// Restrict the sweep to sessions deleted by the current clean operation.
    ThisRun(&'session_ids SessionIds),
    /// Consider every conforming storage file whose session is absent.
    AllOrphans,
}

/// One storage file that could not be removed.
#[derive(Debug)]
pub struct SweepFileError {
    pub path: PathBuf,
    pub source: std::io::Error,
}

/// Counts produced by a storage orphan sweep.
#[derive(Debug, Default)]
pub struct SweepReport {
    pub deleted_files: u64,
    pub non_conforming_files: u64,
    pub retained_live_session_files: u64,
    pub file_errors: Vec<SweepFileError>,
}

/// Removes orphaned session storage files after the caller has committed database mutations.
///
/// The function performs no database writes. It queries the specific session immediately before
/// each unlink, so a live session always protects its storage file even when the caller invokes the
/// sweep before a pending session deletion commits.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when a session existence query fails or [`Error::Io`] when an existing
/// storage directory cannot be enumerated.
pub fn sweep<Access>(
    database: &DatabaseConnection<Access>,
    storage_root: &Path,
    scope: SweepScope<'_>,
) -> Result<SweepReport, Error> {
    let Some(storage_directory) =
        AnchoredDir::open(storage_root).map_err(|source| io_error(storage_root, source))?
    else {
        return Ok(SweepReport::default());
    };

    let mut report = SweepReport::default();
    for bucket_name in storage_directory
        .entries()
        .map_err(|source| io_error(storage_root, source))?
    {
        let bucket_path = storage_root.join(&bucket_name);
        #[cfg(test)]
        run_before_bucket_open_hook(&bucket_path);
        let Some(bucket_directory) = storage_directory
            .open_child(&bucket_name)
            .map_err(|source| io_error(&bucket_path, source))?
        else {
            continue;
        };
        for filename in bucket_directory
            .entries()
            .map_err(|source| io_error(&bucket_path, source))?
        {
            if bucket_directory
                .regular_file_len(&filename)
                .map_err(|source| io_error(&bucket_path.join(&filename), source))?
                .is_none()
            {
                continue;
            }
            let path = bucket_path.join(&filename);
            let Some(session_id) = session_id_from_path(&path) else {
                report.non_conforming_files = report.non_conforming_files.saturating_add(1);
                continue;
            };
            if !scope.includes(session_id) {
                continue;
            }
            if session_exists(database.connection(), session_id)? {
                report.retained_live_session_files =
                    report.retained_live_session_files.saturating_add(1);
                continue;
            }
            match bucket_directory.unlink_file(&filename) {
                Ok(()) => report.deleted_files = report.deleted_files.saturating_add(1),
                Err(source) => report.file_errors.push(SweepFileError { path, source }),
            }
        }
    }
    Ok(report)
}

impl SweepScope<'_> {
    fn includes(self, session_id: &str) -> bool {
        match self {
            Self::ThisRun(session_ids) => session_ids.contains(session_id),
            Self::AllOrphans => true,
        }
    }
}

fn session_exists(connection: &Connection, session_id: &str) -> Result<bool, Error> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1)",
            [session_id],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error("re-validating a storage file session", source))
}

pub(crate) fn session_id_from_path(path: &Path) -> Option<&str> {
    let filename = path.file_name()?.to_str()?;
    let session_id = filename.strip_suffix(".json")?;
    is_session_id(session_id).then_some(session_id)
}

fn is_session_id(value: &str) -> bool {
    value.strip_prefix("ses_").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn sqlite_error(context: &str, source: rusqlite::Error) -> Error {
    db::sqlite_error(context, source)
}

#[cfg(all(test, unix))]
#[derive(Debug)]
struct BeforeBucketOpenHook {
    target: PathBuf,
    replacement: PathBuf,
    symlink_target: PathBuf,
}

#[cfg(all(test, unix))]
static BEFORE_BUCKET_OPEN_HOOK: std::sync::Mutex<Option<BeforeBucketOpenHook>> =
    std::sync::Mutex::new(None);

#[cfg(all(test, unix))]
fn run_before_bucket_open_hook(path: &Path) {
    let hook = {
        let mut slot = BEFORE_BUCKET_OPEN_HOOK
            .lock()
            .expect("before-bucket-open hook lock should remain available");
        if slot.as_ref().is_some_and(|hook| hook.target == path) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        fs::rename(path, &hook.replacement).expect("bucket should move before it is reopened");
        std::os::unix::fs::symlink(&hook.symlink_target, path)
            .expect("replacement bucket symlink should be created");
    }
}

#[cfg(all(test, not(unix)))]
fn run_before_bucket_open_hook(_path: &Path) {}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;
    use crate::db::{ConnectionOptions, ReadOnlyConnection, open_read_only};
    use crate::paths::Target;

    const BUCKETS: [&str; 3] = ["agent-usage-reminder", "directory-readme", "session-diff"];
    const THIS_RUN_IDS: [&str; 3] = ["ses_DeletedA", "ses_DeletedB", "ses_DeletedC"];
    const PREEXISTING_IDS: [&str; 5] = [
        "ses_OrphanA",
        "ses_OrphanB",
        "ses_OrphanC",
        "ses_OrphanD",
        "ses_OrphanE",
    ];

    struct Fixture {
        #[cfg_attr(
            not(unix),
            expect(dead_code, reason = "kept alive so the fixture tree outlives the test")
        )]
        directory: TempDir,
        database_path: PathBuf,
        storage_root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("temporary directory should be created");
            let database_path = directory.path().join("opencode.db");
            let connection =
                Connection::open(&database_path).expect("fixture database should open");
            connection
                .execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY NOT NULL);")
                .expect("session table should be created");
            drop(connection);
            Self {
                storage_root: directory.path().join("storage"),
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

        fn insert_session(&self, session_id: &str) {
            Connection::open(&self.database_path)
                .expect("fixture database should reopen")
                .execute("INSERT INTO session (id) VALUES (?1)", [session_id])
                .expect("session should insert");
        }

        fn file(&self, bucket: &str, session_id: &str) -> PathBuf {
            let bucket = self.storage_root.join(bucket);
            fs::create_dir_all(&bucket).expect("storage bucket should be created");
            let path = bucket.join(format!("{session_id}.json"));
            fs::write(&path, b"fixture").expect("storage file should be written");
            path
        }

        fn populate_scope_files(&self) -> Vec<PathBuf> {
            THIS_RUN_IDS
                .iter()
                .chain(PREEXISTING_IDS.iter())
                .enumerate()
                .map(|(index, session_id)| self.file(BUCKETS[index % BUCKETS.len()], session_id))
                .collect()
        }
    }

    fn ids(values: &[&str]) -> SessionIds {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn this_run_and_all_orphans_scopes_delete_three_and_eight_files() {
        let this_run = Fixture::new();
        let this_run_files = this_run.populate_scope_files();
        let report = sweep(
            &this_run.database(),
            &this_run.storage_root,
            SweepScope::ThisRun(&ids(&THIS_RUN_IDS)),
        )
        .expect("this-run sweep should succeed");

        assert_eq!(report.deleted_files, 3);
        assert!(report.file_errors.is_empty());
        for path in &this_run_files[..3] {
            assert!(!path.exists(), "{}", path.display());
        }
        for path in &this_run_files[3..] {
            assert!(path.exists(), "{}", path.display());
        }

        let all_orphans = Fixture::new();
        let all_files = all_orphans.populate_scope_files();
        let report = sweep(
            &all_orphans.database(),
            &all_orphans.storage_root,
            SweepScope::AllOrphans,
        )
        .expect("full orphan sweep should succeed");

        assert_eq!(report.deleted_files, 8);
        assert!(report.file_errors.is_empty());
        assert!(all_files.iter().all(|path| !path.exists()));
    }

    #[test]
    fn leaves_and_counts_non_conforming_regular_files() {
        let fixture = Fixture::new();
        let bucket = fixture.storage_root.join(BUCKETS[0]);
        fs::create_dir_all(&bucket).expect("storage bucket should be created");
        let notes = bucket.join("notes.json");
        fs::write(&notes, b"keep").expect("notes should be written");

        let report = sweep(
            &fixture.database(),
            &fixture.storage_root,
            SweepScope::AllOrphans,
        )
        .expect("sweep should succeed");

        assert_eq!(report.deleted_files, 0);
        assert_eq!(report.non_conforming_files, 1);
        assert!(notes.exists());
    }

    #[test]
    fn filename_shape_is_strict_ascii_alphanumeric_json() {
        let fixture = Fixture::new();
        let bucket = fixture.storage_root.join(BUCKETS[0]);
        fs::create_dir_all(&bucket).expect("storage bucket should be created");
        let invalid = [
            "ses_.json",
            "ses_has_underscore.json",
            "ses-hyphen.json",
            "ses_nonasciié.json",
            "ses_Valid.json.backup",
            "notes.json",
        ];
        for filename in invalid {
            fs::write(bucket.join(filename), b"keep").expect("storage file should be written");
        }
        let valid = bucket.join("ses_AbC123.json");
        fs::write(&valid, b"delete").expect("valid storage file should be written");

        let report = sweep(
            &fixture.database(),
            &fixture.storage_root,
            SweepScope::AllOrphans,
        )
        .expect("strict filename sweep should succeed");

        assert_eq!(report.deleted_files, 1);
        assert_eq!(report.non_conforming_files, 6);
        assert!(!valid.exists());
        assert!(
            invalid
                .iter()
                .all(|filename| bucket.join(filename).exists())
        );
    }

    #[test]
    fn live_session_recheck_protects_files_before_database_commit() {
        let fixture = Fixture::new();
        let session_id = "ses_PendingDelete";
        fixture.insert_session(session_id);
        let path = fixture.file(BUCKETS[0], session_id);
        let database = fixture.database();
        let before = database.data_version().expect("data version should read");

        let report = sweep(
            &database,
            &fixture.storage_root,
            SweepScope::ThisRun(&ids(&[session_id])),
        )
        .expect("pre-commit sweep should succeed");

        assert_eq!(report.deleted_files, 0);
        assert_eq!(report.retained_live_session_files, 1);
        assert!(path.exists());
        assert_eq!(
            database.data_version().expect("data version should reread"),
            before
        );
        assert!(
            database
                .connection()
                .execute("DELETE FROM session", [])
                .is_err()
        );
    }

    #[test]
    fn absent_storage_directory_returns_zero_report() {
        let fixture = Fixture::new();

        let report = sweep(
            &fixture.database(),
            &fixture.storage_root,
            SweepScope::AllOrphans,
        )
        .expect("absent storage should be empty");

        assert_eq!(report.deleted_files, 0);
        assert_eq!(report.non_conforming_files, 0);
        assert_eq!(report.retained_live_session_files, 0);
        assert!(report.file_errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_symlinked_buckets_or_files() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = fixture
            .directory
            .path()
            .join("outside")
            .join("ses_Outside.json");
        fs::create_dir_all(outside.parent().expect("outside parent should exist"))
            .expect("outside directory should be created");
        fs::write(&outside, b"keep").expect("outside file should be written");
        fs::create_dir_all(&fixture.storage_root).expect("storage root should be created");
        symlink(
            outside.parent().expect("outside parent should exist"),
            fixture.storage_root.join("linked-bucket"),
        )
        .expect("bucket symlink should be created");
        let bucket = fixture.storage_root.join(BUCKETS[0]);
        fs::create_dir_all(&bucket).expect("real bucket should be created");
        symlink(&outside, bucket.join("ses_Linked.json")).expect("file symlink should be created");

        let report = sweep(
            &fixture.database(),
            &fixture.storage_root,
            SweepScope::AllOrphans,
        )
        .expect("symlink-safe sweep should succeed");

        assert_eq!(report.deleted_files, 0);
        assert!(outside.exists());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_a_symlinked_storage_root() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = fixture.directory.path().join("outside-storage-root");
        let bucket = outside.join(BUCKETS[0]);
        fs::create_dir_all(&bucket).expect("outside bucket should be created");
        let outside_file = bucket.join("ses_OutsideRoot.json");
        fs::write(&outside_file, b"outside").expect("outside file should be written");
        symlink(&outside, &fixture.storage_root).expect("storage root symlink should be created");

        let report = sweep(
            &fixture.database(),
            &fixture.storage_root,
            SweepScope::AllOrphans,
        )
        .expect("symlinked storage root should be ignored");

        assert_eq!(report.deleted_files, 0);
        assert_eq!(report.non_conforming_files, 0);
        assert_eq!(report.retained_live_session_files, 0);
        assert!(report.file_errors.is_empty());
        assert!(outside_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn bucket_swap_cannot_redirect_deletion_outside_storage_root() {
        let fixture = Fixture::new();
        let bucket = fixture.storage_root.join(BUCKETS[0]);
        let displaced = fixture.directory.path().join("displaced-storage-bucket");
        let outside = fixture.directory.path().join("outside-storage-bucket");
        fs::create_dir_all(&bucket).expect("storage bucket should be created");
        fs::create_dir_all(&outside).expect("outside bucket should be created");
        let original = bucket.join("ses_Original.json");
        let sentinel = outside.join("ses_Outside.json");
        fs::write(&original, b"original").expect("original file should be written");
        fs::write(&sentinel, b"outside").expect("outside sentinel should be written");
        *super::BEFORE_BUCKET_OPEN_HOOK
            .lock()
            .expect("before-bucket-open hook lock should remain available") =
            Some(super::BeforeBucketOpenHook {
                target: bucket,
                replacement: displaced.clone(),
                symlink_target: outside,
            });

        let report = sweep(
            &fixture.database(),
            &fixture.storage_root,
            SweepScope::AllOrphans,
        )
        .expect("bucket substitution should be handled safely");

        assert!(sentinel.exists(), "deletion escaped the storage root");
        assert!(displaced.join("ses_Original.json").exists());
        assert_eq!(report.deleted_files, 0);
    }
}
