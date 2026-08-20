#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use oc_clean::safety::holders::linux::LinuxHolderInspector;
use oc_clean::safety::holders::{Completeness, HolderInspector, Verdict};
use tempfile::TempDir;

const CHILD_PATH_ENV: &str = "OC_CLEAN_HOLDER_TEST_PATH";

struct HoldingChild {
    child: Child,
}

impl HoldingChild {
    fn spawn(path: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().expect("test executable path"))
            .args(["--exact", "holder_child_helper", "--nocapture"])
            .env(CHILD_PATH_ENV, path)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn holder child");
        let stdout = child.stdout.take().expect("child stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            let bytes = reader.read_line(&mut line).expect("read child readiness");
            assert!(bytes > 0, "holder child exited before becoming ready");
            if line.contains("HOLDER_READY") {
                break;
            }
            line.clear();
        }
        Self { child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for HoldingChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture() -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("fixture directory");
    let database = directory.path().join("opencode.db");
    fs::write(&database, b"fixture").expect("create fixture database");
    (directory, database)
}

#[test]
fn holder_child_helper() {
    let Ok(path) = std::env::var(CHILD_PATH_ENV) else {
        return;
    };
    let _held_file = File::open(path).expect("open held fixture");
    println!("HOLDER_READY");
    std::io::stdout().flush().expect("flush readiness marker");
    thread::sleep(Duration::from_secs(30));
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
    assert_eq!(
        inspection.completeness,
        Completeness::CompleteForVisibleProcesses
    );
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
        Verdict::NotHeld => {}
        Verdict::CannotDetermine(reason) => panic!("scan failed: {reason}"),
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
