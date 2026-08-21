#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;

use oc_clean::safety::holders::linux::LinuxHolderInspector;
use oc_clean::safety::holders::{Completeness, HolderInspector, Verdict};
use tempfile::TempDir;

#[path = "support/holder.rs"]
mod holder;

use holder::HoldingChild;

fn fixture() -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("fixture directory");
    let database = directory.path().join("opencode.db");
    fs::write(&database, b"fixture").expect("create fixture database");
    (directory, database)
}

#[test]
fn detects_child_holding_database() {
    let (_directory, database) = fixture();
    let child = HoldingChild::spawn(&database);

    let inspection = LinuxHolderInspector::default().inspect(&database);

    let Verdict::Held(holders) = inspection.verdict else {
        panic!("expected held verdict, got {:?}", inspection.verdict);
    };
    assert!(holders.iter().any(|holder| holder.pid == child.pid()));
    // A root scan reads every descriptor table and reports complete coverage; an unprivileged
    // scan cannot read other users' processes and degrades to partial. Detecting our own child
    // works in both cases, so only an unsupported scan would be a real failure here.
    assert!(matches!(
        inspection.completeness,
        Completeness::CompleteForVisibleProcesses | Completeness::PartialDueToPermissions
    ));
}

#[test]
fn detects_child_holding_wal_sibling() {
    let (_directory, database) = fixture();
    let wal = PathBuf::from(format!("{}-wal", database.display()));
    fs::write(&wal, b"wal").expect("create WAL fixture");
    let child = HoldingChild::spawn(&wal);

    let inspection = LinuxHolderInspector::default().inspect(&database);

    let Verdict::Held(holders) = inspection.verdict else {
        panic!("expected held verdict, got {:?}", inspection.verdict);
    };
    assert!(holders.iter().any(|holder| {
        holder.pid == child.pid() && holder.matched_paths.iter().any(|path| path == &wal)
    }));
}

#[test]
fn detects_child_holding_shm_sibling() {
    let (_directory, database) = fixture();
    let shm = PathBuf::from(format!("{}-shm", database.display()));
    fs::write(&shm, b"shm").expect("create SHM fixture");
    let child = HoldingChild::spawn(&shm);

    let inspection = LinuxHolderInspector::default().inspect(&database);

    let Verdict::Held(holders) = inspection.verdict else {
        panic!("expected held verdict, got {:?}", inspection.verdict);
    };
    assert!(holders.iter().any(|holder| {
        holder.pid == child.pid() && holder.matched_paths.iter().any(|path| path == &shm)
    }));
}

#[test]
fn closed_handle_is_not_reported() {
    let (_directory, database) = fixture();
    let child_pid = {
        let child = HoldingChild::spawn(&database);
        child.pid()
    };

    let inspection = LinuxHolderInspector::default().inspect(&database);

    match inspection.verdict {
        Verdict::Held(holders) => {
            assert!(holders.iter().all(|holder| holder.pid != child_pid));
        }
        // NotHeld and an indeterminate unprivileged scan both report no holder for the closed
        // descriptor, which is exactly the property under test.
        Verdict::NotHeld | Verdict::CannotDetermine(_) => {}
    }
}

#[test]
fn nonexistent_proc_root_never_reports_not_held() {
    let (_directory, database) = fixture();
    let inspector = LinuxHolderInspector::with_proc_root(PathBuf::from("/missing/proc/root"));

    let inspection = inspector.inspect(&database);

    assert!(matches!(inspection.verdict, Verdict::CannotDetermine(_)));
    assert!(matches!(
        inspection.completeness,
        Completeness::PartialDueToPermissions | Completeness::Unsupported
    ));
}
