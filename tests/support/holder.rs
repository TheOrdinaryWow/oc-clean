use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

const CHILD_PATH_ENV: &str = "OC_CLEAN_HOLDER_TEST_PATH";

pub struct HoldingChild {
    child: Child,
}

impl HoldingChild {
    pub fn spawn(path: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().expect("test executable path"))
            .args(["--exact", "holder::holder_child_helper", "--nocapture"])
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

    #[allow(dead_code)]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for HoldingChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
