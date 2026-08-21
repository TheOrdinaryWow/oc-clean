use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::InterruptHandle;

use crate::error::Error;
use crate::reclaim::vacuum_into::SwapObserver;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Normal,
    Delete,
    Reclaim,
    Swap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignalAction {
    Continue,
    InterruptStatement,
    AbortImmediately,
    Deferred,
}

struct State {
    mode: Mode,
    interrupt: Option<InterruptHandle>,
}

#[derive(Clone)]
pub(super) struct SignalController {
    cancelled: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
}

impl SignalController {
    pub(super) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(State {
                mode: Mode::Normal,
                interrupt: None,
            })),
        }
    }

    pub(super) fn install(&self) -> Result<(), Error> {
        let controller = self.clone();
        ctrlc::set_handler(move || match controller.note_signal() {
            SignalAction::Continue => {}
            SignalAction::InterruptStatement => controller.interrupt_statement(),
            SignalAction::AbortImmediately => std::process::exit(8),
            SignalAction::Deferred => eprintln!("completing atomic swap, do not force-kill"),
        })
        .map_err(|source| Error::Io {
            path: PathBuf::from("<signal-handler>"),
            source: std::io::Error::other(source),
        })
    }

    pub(super) fn set_interrupt_handle(&self, interrupt: InterruptHandle) {
        self.state.lock().expect("signal state poisoned").interrupt = Some(interrupt);
    }

    pub(super) fn begin_delete(&self) {
        self.state.lock().expect("signal state poisoned").mode = Mode::Delete;
    }

    pub(super) fn begin_reclaim(&self) {
        self.state.lock().expect("signal state poisoned").mode = Mode::Reclaim;
    }

    pub(super) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn note_signal(&self) -> SignalAction {
        let state = self.state.lock().expect("signal state poisoned");
        if state.mode == Mode::Swap {
            self.cancelled.store(true, Ordering::Release);
            return SignalAction::Deferred;
        }
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return match state.mode {
                Mode::Delete | Mode::Normal => SignalAction::AbortImmediately,
                Mode::Reclaim => SignalAction::InterruptStatement,
                Mode::Swap => SignalAction::Deferred,
            };
        }
        if state.mode == Mode::Reclaim {
            SignalAction::InterruptStatement
        } else {
            SignalAction::Continue
        }
    }

    fn interrupt_statement(&self) {
        if let Some(interrupt) = &self.state.lock().expect("signal state poisoned").interrupt {
            interrupt.interrupt();
        }
    }
}

impl SwapObserver for SignalController {
    fn enter(&self) -> Result<(), Error> {
        let mut state = self.state.lock().expect("signal state poisoned");
        if self.cancelled() {
            return Err(Error::Interrupted {
                completed: "VACUUM output removed before atomic swap".to_owned(),
            });
        }
        state.mode = Mode::Swap;
        Ok(())
    }

    fn leave(&self) {
        self.state.lock().expect("signal state poisoned").mode = Mode::Reclaim;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use rusqlite::{Connection, ErrorCode};

    use super::*;

    #[test]
    fn second_delete_signal_requests_immediate_abort() {
        let controller = SignalController::new();
        controller.begin_delete();

        assert_eq!(controller.note_signal(), SignalAction::Continue);
        assert_eq!(controller.note_signal(), SignalAction::AbortImmediately);
    }

    #[test]
    fn every_signal_inside_swap_is_deferred() {
        let controller = SignalController::new();
        controller.begin_reclaim();
        controller.enter().expect("swap should start");

        assert_eq!(controller.note_signal(), SignalAction::Deferred);
        assert_eq!(controller.note_signal(), SignalAction::Deferred);
        controller.leave();
        assert!(controller.cancelled());
    }

    #[test]
    fn repeated_reclaim_signal_keeps_interrupting_for_temporary_cleanup() {
        let controller = SignalController::new();
        controller.begin_reclaim();

        assert_eq!(controller.note_signal(), SignalAction::InterruptStatement);
        assert_eq!(controller.note_signal(), SignalAction::InterruptStatement);
    }

    #[test]
    fn reclaim_signal_interrupts_an_inflight_sqlite_statement() {
        let connection = Connection::open_in_memory().expect("SQLite should open");
        let controller = SignalController::new();
        controller.set_interrupt_handle(connection.get_interrupt_handle());
        controller.begin_reclaim();
        let (started_tx, started_rx) = mpsc::channel();
        let query = thread::spawn(move || {
            started_tx.send(()).expect("test receiver should remain");
            connection.query_row(
                "WITH RECURSIVE counter(value) AS (
                    VALUES(0) UNION ALL SELECT value + 1 FROM counter WHERE value < 1000000000
                 ) SELECT sum(value) FROM counter",
                [],
                |row| row.get::<_, i64>(0),
            )
        });
        started_rx.recv().expect("query should start");
        thread::sleep(Duration::from_millis(20));

        assert_eq!(controller.note_signal(), SignalAction::InterruptStatement);
        controller.interrupt_statement();
        let error = query
            .join()
            .expect("query thread should join")
            .expect_err("statement should be interrupted");

        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(ref failure, _)
                if failure.code == ErrorCode::OperationInterrupted
        ));
    }
}
