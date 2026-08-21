//! Cross-platform process-holder inspection and command gating.

#[cfg(unix)]
use std::collections::{HashSet, VecDeque};
#[cfg(unix)]
use std::ffi::OsStr;
use std::ffi::OsString;
#[cfg(unix)]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use rustix::fs::{Mode, OFlags, openat, readlinkat};
#[cfg(unix)]
use rustix::io::Errno;

use crate::db::anchor_for_holder_scan;
use crate::error::Error;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// A process with one or more open database-related files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HolderInfo {
    /// Operating-system process identifier.
    pub pid: u32,
    /// Best-effort process name.
    pub process_name: Option<String>,
    /// Database, WAL, or SHM paths held by this process.
    pub matched_paths: Vec<PathBuf>,
}

/// Whether a scan covered all processes visible to the current user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completeness {
    /// Every visible process was inspected.
    CompleteForVisibleProcesses,
    /// Permission restrictions prevented inspection of at least one process.
    PartialDueToPermissions,
    /// The platform or process API could not perform a meaningful scan.
    Unsupported,
}

/// Three-state result of inspecting database holders.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// At least one process holds the database, WAL, or SHM file.
    Held(Vec<HolderInfo>),
    /// The complete visible-process scan found no holders.
    NotHeld,
    /// The scan could not establish whether any holder exists.
    CannotDetermine(String),
}

/// Holder verdict paired with scan coverage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inspection {
    /// Holder result.
    pub verdict: Verdict,
    /// Coverage achieved by the inspector.
    pub completeness: Completeness,
}

/// Platform implementation capable of finding open database files.
pub trait HolderInspector {
    /// Inspects the main database path and its `-wal` and `-shm` siblings.
    fn inspect(&self, database_path: &Path) -> Inspection;
}

/// Command mode whose holder policy must be evaluated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandMode {
    /// Read-only analysis.
    Analyze,
    /// Read-only diagnostics.
    Doctor,
    /// Cleanup preview or application.
    Clean { apply: bool },
    /// Vacuum preview or application.
    Vacuum { apply: bool },
}

/// Action selected by the holder gating matrix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GateDecision {
    /// Continue without a holder warning.
    Proceed,
    /// Continue after reporting the holder state.
    Warn,
    /// Refuse a destructive operation because holders were observed.
    RefuseHeld(Vec<HolderInfo>),
    /// Refuse a destructive operation because holder status is unknown.
    RefuseCannotDetermine(String),
}

impl GateDecision {
    /// Converts a destructive refusal into the stable database-busy error category.
    ///
    /// Warning and proceed decisions remain successful so read-only commands and forced writes can
    /// continue after the caller reports the inspection details.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DatabaseBusy`] for `RefuseHeld` and `RefuseCannotDetermine`.
    pub fn into_result(self) -> Result<(), Error> {
        match self {
            Self::Proceed | Self::Warn => Ok(()),
            Self::RefuseHeld(holders) => Err(Error::DatabaseBusy {
                holders: holders.iter().map(holder_description).collect(),
            }),
            Self::RefuseCannotDetermine(reason) => Err(Error::DatabaseBusy {
                holders: vec![format!("cannot determine process holders: {reason}")],
            }),
        }
    }
}

fn holder_description(holder: &HolderInfo) -> String {
    let name = holder.process_name.as_deref().unwrap_or("unknown process");
    let paths = holder
        .matched_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("pid {} ({name}) holds {paths}", holder.pid)
}

/// Runs an inspector and applies the command's holder policy.
#[must_use]
pub fn inspect_and_decide(
    inspector: &dyn HolderInspector,
    database_path: &Path,
    command: CommandMode,
    force: bool,
) -> (Inspection, GateDecision) {
    let anchor = match anchor_for_holder_scan(database_path) {
        Ok(anchor) => Some(anchor),
        Err(Error::NotFound { .. }) => None,
        Err(error @ Error::DatabaseBusy { .. }) => {
            return holder_target_change(database_path, &error);
        }
        Err(error) => {
            return holder_resolution_failure(database_path, &error, command, force);
        }
    };
    let inspection_path = anchor.as_deref().map_or_else(
        || database_path.to_path_buf(),
        crate::db::AnchoredDatabaseFile::inspection_path,
    );
    let mut inspection = inspector.inspect(&inspection_path);
    if let Some(anchor) = anchor.as_deref() {
        remap_holder_paths(&mut inspection, &inspection_path, database_path);
        if let Err(error) = anchor.ensure_path_matches(
            database_path,
            "database target changed during holder inspection",
        ) {
            return holder_target_change(database_path, &error);
        }
    }
    decide(inspection, command, force)
}

fn holder_target_change(database_path: &Path, error: &Error) -> (Inspection, GateDecision) {
    let reason = format!(
        "database target {} changed during holder inspection: {error}",
        database_path.display()
    );
    (
        Inspection {
            verdict: Verdict::CannotDetermine(reason.clone()),
            completeness: Completeness::Unsupported,
        },
        GateDecision::RefuseCannotDetermine(reason),
    )
}

fn remap_holder_paths(inspection: &mut Inspection, resolved_path: &Path, lexical_path: &Path) {
    let resolved = database_related_paths(resolved_path);
    let lexical = database_related_paths(lexical_path);
    if let Verdict::Held(holders) = &mut inspection.verdict {
        for holder in holders {
            for matched_path in &mut holder.matched_paths {
                if let Some(index) = resolved.iter().position(|path| path == matched_path) {
                    matched_path.clone_from(&lexical[index]);
                }
            }
        }
    }
}

fn holder_resolution_failure(
    database_path: &Path,
    error: &Error,
    command: CommandMode,
    force: bool,
) -> (Inspection, GateDecision) {
    decide(
        Inspection {
            verdict: Verdict::CannotDetermine(format!(
                "cannot anchor database target {}: {error}",
                database_path.display()
            )),
            completeness: Completeness::Unsupported,
        },
        command,
        force,
    )
}

fn decide(
    mut inspection: Inspection,
    command: CommandMode,
    force: bool,
) -> (Inspection, GateDecision) {
    if let Verdict::Held(holders) = &mut inspection.verdict {
        holders.retain(|holder| holder.pid != std::process::id());
        if holders.is_empty() {
            inspection.verdict = match inspection.completeness {
                Completeness::CompleteForVisibleProcesses => Verdict::NotHeld,
                Completeness::PartialDueToPermissions | Completeness::Unsupported => {
                    Verdict::CannotDetermine(
                        "holder inspection could not establish whether another process holds the database"
                            .to_owned(),
                    )
                }
            };
        }
    }
    let destructive = matches!(
        command,
        CommandMode::Clean { apply: true } | CommandMode::Vacuum { apply: true }
    );
    let decision = match &inspection.verdict {
        Verdict::NotHeld => GateDecision::Proceed,
        Verdict::Held(holders) if destructive && !force => {
            GateDecision::RefuseHeld(holders.clone())
        }
        Verdict::CannotDetermine(reason)
            if destructive && !force && blocks_destructive_work(inspection.completeness) =>
        {
            GateDecision::RefuseCannotDetermine(reason.clone())
        }
        Verdict::Held(_) | Verdict::CannotDetermine(_) => GateDecision::Warn,
    };
    (inspection, decision)
}

/// Reports whether an indeterminate scan is too weak to allow destructive work.
///
/// A scan blinded by permissions still covered every process the account can see, which is exactly
/// the coverage [`Completeness::CompleteForVisibleProcesses`] promises. An unprivileged account can
/// never read another user's descriptor table, so refusing on that basis alone would block every
/// non-root invocation while proving nothing. A scan the platform could not perform at all
/// establishes no coverage, so it keeps refusing.
const fn blocks_destructive_work(completeness: Completeness) -> bool {
    match completeness {
        Completeness::Unsupported => true,
        Completeness::CompleteForVisibleProcesses | Completeness::PartialDueToPermissions => false,
    }
}

pub(crate) fn database_related_paths(database_path: &Path) -> [PathBuf; 3] {
    let database_path = normalize_database_path(database_path);
    [
        database_path.clone(),
        path_with_suffix(&database_path, "-wal"),
        path_with_suffix(&database_path, "-shm"),
    ]
}

#[cfg(any(target_os = "linux", all(unix, test)))]
pub(crate) fn resolve_database_target(database_path: &Path) -> io::Result<PathBuf> {
    open_database_target(database_path).map(|target| target.resolved_path)
}

#[cfg(not(unix))]
pub(crate) fn resolve_database_target(database_path: &Path) -> io::Result<PathBuf> {
    resolve_database_target_by_path(database_path)
}

#[cfg(unix)]
pub(crate) struct OpenedDatabaseTarget {
    pub(crate) descriptor: File,
    pub(crate) parent_descriptor: File,
    pub(crate) file_name: OsString,
    pub(crate) resolved_path: PathBuf,
}

#[cfg(unix)]
pub(crate) fn open_database_target(database_path: &Path) -> io::Result<OpenedDatabaseTarget> {
    const MAX_SYMLINK_HOPS: usize = 40;

    let absolute = normalize_database_path(database_path);
    let mut pending = path_components(&absolute);
    let mut resolved_components = Vec::<OsString>::new();
    let mut visited = HashSet::with_capacity(MAX_SYMLINK_HOPS);
    let mut hops = 0;
    let mut directory = File::from(openat(
        rustix::fs::CWD,
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);

    while let Some(component) = pending.pop_front() {
        if component == OsStr::new(".") {
            continue;
        }
        if component == OsStr::new("..") {
            directory = File::from(openat(
                &directory,
                "..",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            resolved_components.pop();
            continue;
        }

        match readlinkat(&directory, &component, Vec::new()) {
            Ok(target) => {
                if hops == MAX_SYMLINK_HOPS {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "more than {MAX_SYMLINK_HOPS} symlink hops while resolving {}",
                            database_path.display()
                        ),
                    ));
                }
                let target = PathBuf::from(OsString::from_vec(target.into_bytes()));
                let state = resolution_state(&resolved_components, &component, &pending, &target);
                if !visited.insert(state) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("symlink cycle while resolving {}", database_path.display()),
                    ));
                }
                if target.is_absolute() {
                    directory = File::from(openat(
                        rustix::fs::CWD,
                        "/",
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?);
                    resolved_components.clear();
                }
                prepend_components(&mut pending, &target);
                hops += 1;
            }
            Err(Errno::INVAL) if pending.is_empty() => {
                let descriptor = File::from(openat(
                    &directory,
                    &component,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                let resolved_path = resolved_path(&resolved_components, &component);
                return Ok(OpenedDatabaseTarget {
                    descriptor,
                    parent_descriptor: directory,
                    file_name: component,
                    resolved_path,
                });
            }
            Err(Errno::INVAL) => {
                directory = File::from(openat(
                    &directory,
                    &component,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                resolved_components.push(component);
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("database path {} has no file name", database_path.display()),
    ))
}

#[cfg(unix)]
fn path_components(path: &Path) -> VecDeque<OsString> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_owned()),
            std::path::Component::ParentDir => Some(OsString::from("..")),
            std::path::Component::CurDir => Some(OsString::from(".")),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => None,
        })
        .collect()
}

#[cfg(unix)]
fn prepend_components(pending: &mut VecDeque<OsString>, path: &Path) {
    let mut components = path_components(path);
    components.append(pending);
    *pending = components;
}

#[cfg(unix)]
fn resolved_path(components: &[OsString], file_name: &OsStr) -> PathBuf {
    let mut path = PathBuf::from("/");
    path.extend(components);
    path.push(file_name);
    path
}

#[cfg(unix)]
fn resolution_state(
    resolved: &[OsString],
    component: &OsStr,
    pending: &VecDeque<OsString>,
    target: &Path,
) -> PathBuf {
    let mut state = resolved_path(resolved, component);
    state.push(target);
    state.extend(pending);
    state
}

#[cfg(not(unix))]
fn resolve_database_target_by_path(database_path: &Path) -> io::Result<PathBuf> {
    const MAX_SYMLINK_HOPS: usize = 40;

    let mut current = normalize_database_path(database_path);
    let mut visited = std::collections::HashSet::with_capacity(MAX_SYMLINK_HOPS);
    let mut hops = 0;

    loop {
        if !visited.insert(current.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("symlink cycle while resolving {}", database_path.display()),
            ));
        }
        if !std::fs::symlink_metadata(&current)?
            .file_type()
            .is_symlink()
        {
            return Ok(current);
        }
        if hops == MAX_SYMLINK_HOPS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "more than {MAX_SYMLINK_HOPS} symlink hops while resolving {}",
                    database_path.display()
                ),
            ));
        }

        let target = std::fs::read_link(&current)?;
        let resolved = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .map_or_else(|| target.clone(), |parent| parent.join(&target))
        };
        current = normalize_database_path(&resolved);
        hops += 1;
    }
}

fn normalize_database_path(path: &Path) -> PathBuf {
    fold_path_case(strip_trailing_separators(&absolute_path(path)))
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_owned(), |current| current.join(path))
    }
}

#[cfg(unix)]
fn strip_trailing_separators(path: &Path) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let bytes = path.as_os_str().as_bytes();
    let end = bytes
        .iter()
        .rposition(|byte| *byte != b'/')
        .map_or(1, |index| index + 1);
    PathBuf::from(OsString::from_vec(bytes[..end].to_vec()))
}

#[cfg(windows)]
fn strip_trailing_separators(path: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    while wide
        .last()
        .is_some_and(|unit| *unit == u16::from(b'/') || *unit == u16::from(b'\\'))
    {
        let candidate = OsString::from_wide(&wide[..wide.len() - 1]);
        if !Path::new(&candidate).is_absolute() {
            break;
        }
        wide.pop();
    }
    PathBuf::from(OsString::from_wide(&wide))
}

#[cfg(not(windows))]
fn fold_path_case(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "mirrors the non-windows identity function that returns its owned argument"
)]
fn fold_path_case(path: PathBuf) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut suffixed = path.as_os_str().to_owned();
    suffixed.push(suffix);
    PathBuf::from(suffixed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[derive(Clone)]
    struct FakeInspector(Inspection);

    impl HolderInspector for FakeInspector {
        fn inspect(&self, _database_path: &Path) -> Inspection {
            self.0.clone()
        }
    }

    #[cfg(unix)]
    struct RetargetingInspector {
        link: PathBuf,
    }

    #[cfg(unix)]
    impl HolderInspector for RetargetingInspector {
        fn inspect(&self, _database_path: &Path) -> Inspection {
            std::fs::remove_file(&self.link).expect("old database symlink should be removed");
            symlink("replacement.db", &self.link)
                .expect("replacement database symlink should be created");
            not_held()
        }
    }

    #[cfg(target_os = "linux")]
    struct TransientAncestorSwapInspector {
        live_directory: PathBuf,
        displaced_directory: PathBuf,
        database_name: PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl HolderInspector for TransientAncestorSwapInspector {
        fn inspect(&self, database_path: &Path) -> Inspection {
            std::fs::rename(&self.live_directory, &self.displaced_directory)
                .expect("database directory should be displaced");
            std::fs::create_dir(&self.live_directory)
                .expect("attacker directory should be created");
            std::fs::write(
                self.live_directory.join(&self.database_name),
                b"replacement",
            )
            .expect("attacker database should be created");

            let observed = std::fs::read(database_path).expect("inspection target should open");

            std::fs::remove_dir_all(&self.live_directory)
                .expect("attacker directory should be removed");
            std::fs::rename(&self.displaced_directory, &self.live_directory)
                .expect("database directory should be restored");

            if observed == b"original" {
                held()
            } else {
                not_held()
            }
        }
    }

    fn held() -> Inspection {
        Inspection {
            verdict: Verdict::Held(vec![HolderInfo {
                pid: 42,
                process_name: Some("opencode".to_owned()),
                matched_paths: vec![PathBuf::from("opencode.db")],
            }]),
            completeness: Completeness::CompleteForVisibleProcesses,
        }
    }

    fn not_held() -> Inspection {
        Inspection {
            verdict: Verdict::NotHeld,
            completeness: Completeness::CompleteForVisibleProcesses,
        }
    }

    fn unknown() -> Inspection {
        Inspection {
            verdict: Verdict::CannotDetermine("permission denied".to_owned()),
            completeness: Completeness::PartialDueToPermissions,
        }
    }

    fn unsupported() -> Inspection {
        Inspection {
            verdict: Verdict::CannotDetermine("platform scan unavailable".to_owned()),
            completeness: Completeness::Unsupported,
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_related_paths_preserve_symlink_path() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let target = directory.path().join("target.db");
        std::fs::write(&target, []).expect("database fixture should be created");
        let link = directory.path().join("linked.db");
        symlink(&target, &link).expect("database symlink should be created");

        let related = database_related_paths(&link);

        assert_eq!(related[0], link);
        assert_eq!(related[1], path_with_suffix(&link, "-wal"));
        assert_eq!(related[2], path_with_suffix(&link, "-shm"));
        assert_ne!(related[0], target);
    }

    #[cfg(unix)]
    #[test]
    fn database_target_resolution_rejects_symlink_cycles() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let first = directory.path().join("first.db");
        let second = directory.path().join("second.db");
        symlink("second.db", &first).expect("first symlink should be created");
        symlink("first.db", &second).expect("second symlink should be created");

        let error = resolve_database_target(&first).expect_err("symlink cycle should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("symlink cycle"));
    }

    #[cfg(unix)]
    #[test]
    fn holder_scan_rejects_symlink_retarget_during_inspection() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let original = directory.path().join("original.db");
        let replacement = directory.path().join("replacement.db");
        let link = directory.path().join("opencode.db");
        std::fs::write(&original, b"original").expect("original fixture should be created");
        std::fs::write(&replacement, b"replacement")
            .expect("replacement fixture should be created");
        symlink("original.db", &link).expect("database symlink should be created");

        let (inspection, decision) = inspect_and_decide(
            &RetargetingInspector { link: link.clone() },
            &link,
            CommandMode::Vacuum { apply: true },
            true,
        );

        assert!(matches!(inspection.verdict, Verdict::CannotDetermine(_)));
        assert!(matches!(decision, GateDecision::RefuseCannotDetermine(_)));
        assert_eq!(
            decision
                .into_result()
                .expect_err("retargeted holder scan should abort")
                .exit_code(),
            5
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn holder_scan_reads_through_pinned_descriptor_during_ancestor_swap() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let live_directory = directory.path().join("live");
        let displaced_directory = directory.path().join("displaced");
        let database_name = PathBuf::from("opencode.db");
        std::fs::create_dir(&live_directory).expect("database directory should be created");
        let database_path = live_directory.join(&database_name);
        std::fs::write(&database_path, b"original").expect("database fixture should be created");

        let (inspection, _) = inspect_and_decide(
            &TransientAncestorSwapInspector {
                live_directory,
                displaced_directory,
                database_name,
            },
            &database_path,
            CommandMode::Vacuum { apply: true },
            true,
        );

        assert!(matches!(inspection.verdict, Verdict::Held(_)));
    }

    /// Counts the symlink hops the resolver spends walking to `directory`.
    #[cfg(unix)]
    fn symlink_hops_in_prefix(directory: &Path) -> usize {
        let mut hops = 0;
        let mut walked = PathBuf::from("/");
        for component in directory.components().skip(1) {
            walked.push(component);
            if std::fs::symlink_metadata(&walked)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                hops += 1;
            }
        }
        hops
    }

    #[cfg(unix)]
    #[test]
    fn database_target_resolution_allows_forty_and_rejects_more_symlink_hops() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let target = directory.path().join("real.db");
        std::fs::write(&target, []).expect("database fixture should be created");

        // The directory prefix may itself contain symlinks, and those hops come out of the same
        // budget. macOS resolves the temporary directory through /var -> /private/var, so build
        // the chain relative to whatever the prefix already spent.
        let prefix_hops = symlink_hops_in_prefix(directory.path());
        let chain_length = 40 - prefix_hops;

        for index in (0..=chain_length).rev() {
            let link = directory.path().join(format!("link-{index}.db"));
            let target_name = if index == chain_length {
                OsString::from("real.db")
            } else {
                OsString::from(format!("link-{}.db", index + 1))
            };
            symlink(target_name, link).expect("symlink chain should be created");
        }

        let resolved = resolve_database_target(&directory.path().join("link-1.db"))
            .expect("forty symlink hops should resolve");
        assert_eq!(resolved, target);

        let error = resolve_database_target(&directory.path().join("link-0.db"))
            .expect_err("overlong symlink chain should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("more than 40 symlink hops"));
    }

    #[test]
    fn database_related_paths_are_absolute_and_strip_trailing_separators() {
        let related = database_related_paths(Path::new("holder-fixture.db/"));

        assert!(related[0].is_absolute());
        assert_eq!(
            related[0].file_name(),
            Some(std::ffi::OsStr::new("holder-fixture.db"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn database_related_paths_fold_case_on_windows() {
        let related = database_related_paths(Path::new(r"C:\Holder-Fixture.DB\"));

        assert_eq!(related[0], Path::new(r"c:\holder-fixture.db"));
    }

    #[test]
    fn gating_matrix_is_table_driven() {
        let states = [held(), not_held(), unknown(), unsupported()];
        let commands = [
            CommandMode::Analyze,
            CommandMode::Doctor,
            CommandMode::Clean { apply: false },
            CommandMode::Vacuum { apply: false },
            CommandMode::Clean { apply: true },
            CommandMode::Vacuum { apply: true },
        ];

        for state in states {
            for command in commands {
                let fake = FakeInspector(state.clone());
                let (_, decision) =
                    inspect_and_decide(&fake, Path::new("opencode.db"), command, false);
                let destructive = matches!(
                    command,
                    CommandMode::Clean { apply: true } | CommandMode::Vacuum { apply: true }
                );
                match (&state.verdict, destructive) {
                    (Verdict::Held(_), true) => {
                        assert!(matches!(decision, GateDecision::RefuseHeld(_)));
                    }
                    (Verdict::CannotDetermine(_), true)
                        if state.completeness == Completeness::Unsupported =>
                    {
                        assert!(matches!(decision, GateDecision::RefuseCannotDetermine(_)));
                    }
                    (Verdict::NotHeld, _) => assert_eq!(decision, GateDecision::Proceed),
                    (Verdict::Held(_) | Verdict::CannotDetermine(_), _) => {
                        assert_eq!(decision, GateDecision::Warn);
                    }
                }
            }
        }
    }

    #[test]
    fn permission_limited_scan_warns_instead_of_refusing_destructive_work() {
        for command in [
            CommandMode::Clean { apply: true },
            CommandMode::Vacuum { apply: true },
        ] {
            let (inspection, decision) = inspect_and_decide(
                &FakeInspector(unknown()),
                Path::new("opencode.db"),
                command,
                false,
            );

            assert_eq!(
                inspection.completeness,
                Completeness::PartialDueToPermissions
            );
            assert!(matches!(inspection.verdict, Verdict::CannotDetermine(_)));
            assert_eq!(decision, GateDecision::Warn);
            assert!(decision.into_result().is_ok());
        }
    }

    #[test]
    fn unsupported_scan_still_refuses_destructive_work() {
        let (_, decision) = inspect_and_decide(
            &FakeInspector(unsupported()),
            Path::new("opencode.db"),
            CommandMode::Clean { apply: true },
            false,
        );

        assert!(matches!(decision, GateDecision::RefuseCannotDetermine(_)));
        assert_eq!(
            decision
                .into_result()
                .expect_err("unsupported scan must refuse cleanup")
                .exit_code(),
            5
        );
    }

    #[test]
    fn force_bypasses_both_destructive_refusals() {
        for state in [held(), unsupported()] {
            let fake = FakeInspector(state);
            for command in [
                CommandMode::Clean { apply: true },
                CommandMode::Vacuum { apply: true },
            ] {
                let (_, decision) =
                    inspect_and_decide(&fake, Path::new("opencode.db"), command, true);
                assert_eq!(decision, GateDecision::Warn);
            }
        }
    }

    #[test]
    fn destructive_refusals_map_to_distinct_database_busy_errors() {
        let held_error = inspect_and_decide(
            &FakeInspector(held()),
            Path::new("opencode.db"),
            CommandMode::Clean { apply: true },
            false,
        )
        .1
        .into_result()
        .expect_err("held database must refuse cleanup");
        let unknown_error = inspect_and_decide(
            &FakeInspector(unsupported()),
            Path::new("opencode.db"),
            CommandMode::Vacuum { apply: true },
            false,
        )
        .1
        .into_result()
        .expect_err("unknown holder state must refuse vacuum");

        assert_eq!(held_error.exit_code(), 5);
        assert_eq!(unknown_error.exit_code(), 5);
        assert!(held_error.to_string().contains("pid 42"));
        assert!(
            unknown_error
                .to_string()
                .contains("cannot determine process holders: platform scan unavailable")
        );
    }
}
