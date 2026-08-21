//! Bounded parallel execution for independent read-only work.
//!
//! Every command in this tool runs a handful of independent, blocking SQLite and filesystem
//! probes. Those are exactly the workload `std::thread::scope` handles well: the jobs borrow
//! stack data, they block in C code rather than awaiting, and cancellation is already handled
//! by the SIGINT path rather than by unwinding a task tree. An async runtime would have to
//! push each of them onto a blocking pool anyway, so the threads are kept explicit.
//!
//! Two properties matter more than raw speed:
//!
//! - **Bounded concurrency.** Concurrent readers help even on a rotating disk because the IO
//!   scheduler can merge seeks, but only while the count stays near the core count. The gate
//!   caps in-flight jobs at `min(job count, available parallelism)`.
//! - **Deterministic failure.** Exit codes are a stable contract, so when several jobs fail
//!   the reported error must not depend on which thread finished first. Callers join in
//!   declaration order, which makes the surfaced failure the first declared one every time.

use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::error::Error;
use crate::report::progress;

/// Returns the number of jobs that may run at once for a group of `job_count` jobs.
///
/// Never exceeds the job count, because idle permits cost nothing, and never exceeds available
/// parallelism, because oversubscribed readers turn merged sequential IO back into seek thrash.
#[must_use]
pub fn permits(job_count: usize) -> usize {
    let cores = thread::available_parallelism().map_or(1, NonZeroUsize::get);
    job_count.min(cores).max(1)
}

/// Runs `build` with a group that can spawn bounded, labelled parallel jobs.
///
/// The group waits for every spawned job before returning, including on the early-return path
/// taken when a caller propagates a failure with `?`.
pub fn group<'env, Build, T>(prefix: &'static str, job_count: usize, build: Build) -> T
where
    Build: for<'scope> FnOnce(&Group<'scope, 'env>) -> T,
{
    let gate = Arc::new(Gate::new(permits(job_count)));
    thread::scope(|scope: &thread::Scope<'_, 'env>| {
        let group = Group {
            scope,
            gate: Arc::clone(&gate),
            prefix,
        };
        build(&group)
    })
}

/// Spawns bounded parallel jobs that borrow from the enclosing scope.
pub struct Group<'scope, 'env: 'scope> {
    scope: &'scope thread::Scope<'scope, 'env>,
    gate: Arc<Gate>,
    prefix: &'static str,
}

impl<'scope, 'env: 'scope> Group<'scope, 'env> {
    /// Spawns `job`, rendering `label` on its own spinner while it runs.
    ///
    /// The spinner is created inside the worker after it acquires a permit, so the terminal
    /// shows the jobs that are actually running rather than the ones merely queued.
    pub fn spawn<Job, T>(&self, label: &'static str, job: Job) -> JobHandle<'scope, T>
    where
        Job: FnOnce() -> Result<T, Error> + Send + 'scope,
        T: Send + 'scope,
    {
        let gate = Arc::clone(&self.gate);
        let prefix = self.prefix;
        JobHandle {
            handle: self.scope.spawn(move || {
                let _permit = gate.acquire();
                let bar = progress::spinner(prefix, label);
                let outcome = job();
                bar.finish();
                outcome
            }),
            label,
        }
    }
}

/// A handle to one spawned job.
pub struct JobHandle<'scope, T> {
    handle: thread::ScopedJoinHandle<'scope, Result<T, Error>>,
    label: &'static str,
}

impl<T> JobHandle<'_, T> {
    /// Waits for the job and returns its outcome.
    ///
    /// # Errors
    ///
    /// Returns the job's own error, or [`Error::InvalidArgument`] naming the job when its
    /// thread panicked. A panic is a defect rather than a supported outcome, so it is reported
    /// instead of resumed: unwinding through the group would abandon the sibling jobs' bars.
    pub fn join(self) -> Result<T, Error> {
        match self.handle.join() {
            Ok(outcome) => outcome,
            Err(_) => Err(Error::InvalidArgument {
                argument: self.label.to_owned(),
                reason: "parallel job panicked".to_owned(),
            }),
        }
    }
}

/// A counting semaphore bounding how many jobs execute at once.
struct Gate {
    available: Mutex<usize>,
    released: Condvar,
}

impl Gate {
    fn new(permits: usize) -> Self {
        Self {
            available: Mutex::new(permits),
            released: Condvar::new(),
        }
    }

    fn acquire(&self) -> Permit<'_> {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *available == 0 {
            available = self
                .released
                .wait(available)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *available -= 1;
        Permit { gate: self }
    }
}

/// Returns its permit to the gate when dropped, including while a panic unwinds.
struct Permit<'gate> {
    gate: &'gate Gate,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut available = self
            .gate
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available += 1;
        self.gate.released.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Gate, group, permits};
    use crate::error::Error;

    #[test]
    fn permits_never_exceed_the_job_count_or_drop_below_one() {
        assert_eq!(permits(0), 1);
        assert_eq!(permits(1), 1);
        assert!(permits(3) <= 3);
        assert!(permits(1_000) <= super::thread::available_parallelism().unwrap().get());
    }

    #[test]
    fn every_job_runs_and_results_are_returned_in_declaration_order() {
        let outcome: Result<Vec<u32>, Error> = group("test", 4, |group| {
            let jobs: Vec<_> = (0..4_u32)
                .map(|index| group.spawn("job", move || Ok(index)))
                .collect();
            jobs.into_iter().map(super::JobHandle::join).collect()
        });

        assert_eq!(outcome.expect("all jobs succeed"), vec![0, 1, 2, 3]);
    }

    #[test]
    fn the_surfaced_failure_is_the_first_declared_one_not_the_first_finished() {
        // The later job fails immediately while the earlier one sleeps, so a "first to
        // finish" policy would surface `second`. Declaration order must win instead.
        let outcome: Result<(), Error> = group("test", 2, |group| {
            let first = group.spawn("first", || -> Result<(), Error> {
                std::thread::sleep(std::time::Duration::from_millis(60));
                Err(Error::InvalidArgument {
                    argument: "first".to_owned(),
                    reason: "slow failure".to_owned(),
                })
            });
            let second = group.spawn("second", || -> Result<(), Error> {
                Err(Error::InvalidArgument {
                    argument: "second".to_owned(),
                    reason: "fast failure".to_owned(),
                })
            });
            let first = first.join();
            let second = second.join();
            first.and(second)
        });

        let Err(Error::InvalidArgument { argument, .. }) = outcome else {
            panic!("the group should surface a failure");
        };
        assert_eq!(argument, "first");
    }

    #[test]
    fn concurrency_never_exceeds_the_permit_count() {
        let limit = permits(8);
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        let outcome: Result<Vec<()>, Error> = group("test", 8, |group| {
            let jobs: Vec<_> = (0..8)
                .map(|_| {
                    let in_flight = &in_flight;
                    let peak = &peak;
                    group.spawn("job", move || {
                        let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(current, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                })
                .collect();
            jobs.into_iter().map(super::JobHandle::join).collect()
        });

        outcome.expect("all jobs succeed");
        assert!(
            peak.load(Ordering::SeqCst) <= limit,
            "peak concurrency {} exceeded the {limit} permit budget",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn a_panicking_job_is_reported_without_unwinding_the_group() {
        let outcome: Result<(), Error> = group("test", 1, |group| {
            group
                .spawn("exploding", || -> Result<(), Error> { panic!("boom") })
                .join()
        });

        let Err(Error::InvalidArgument { argument, reason }) = outcome else {
            panic!("a panicking job should surface as a failure");
        };
        assert_eq!(argument, "exploding");
        assert!(reason.contains("panicked"));
    }

    #[test]
    fn a_permit_is_returned_even_when_its_holder_panics() {
        let gate = Gate::new(1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _permit = gate.acquire();
            panic!("boom");
        }));

        assert!(result.is_err());
        assert_eq!(
            *gate
                .available
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            1,
            "the permit must return to the gate during unwinding"
        );
    }
}
