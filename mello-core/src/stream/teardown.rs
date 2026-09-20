//! Bounded native teardown.
//!
//! Native stop calls (`mello_stream_stop_host`, `mello_stream_stop_viewer`,
//! `mello_peer_destroy`) join capture, encode and network threads inside
//! libmello. A driver or a capture thread that never returns makes that join
//! block forever. On 2026-09-15 a beta host's `mello_stream_stop_host` never
//! returned. It ran on the core command loop, so voice receive, hangup and END
//! STREAM all stopped with it.
//!
//! Every native teardown now runs on its own OS thread, never on a tokio worker
//! and never awaited by the command loop. A watchdog logs the step that is
//! still running when the deadline passes, and reports it through the hang
//! hook. A hung teardown keeps its thread and the resources it owns forever:
//! freeing callback contexts while native threads still run would be a
//! use-after-free, and a leak is the correct failure.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Default time a native teardown may take before it is reported as hung.
pub const TEARDOWN_DEADLINE: Duration = Duration::from_secs(5);

/// Teardowns that passed their deadline and have not finished yet.
static HUNG_TEARDOWNS: AtomicUsize = AtomicUsize::new(0);

/// A native teardown that passed its deadline.
#[derive(Debug, Clone)]
pub struct TeardownHang {
    /// What was being torn down, for example `stream_host`.
    pub label: &'static str,
    /// The step that had started and not finished.
    pub step: &'static str,
    /// Time since the teardown started.
    pub elapsed: Duration,
    /// Teardowns hung at this moment, this one included.
    pub hung_total: usize,
}

type HangHook = Box<dyn Fn(&TeardownHang) + Send + Sync>;
static HANG_HOOK: OnceLock<HangHook> = OnceLock::new();

/// Install the process-wide hook that runs when a teardown passes its deadline.
///
/// The client uses it for telemetry. Only the first call has an effect.
pub fn set_hang_hook(hook: impl Fn(&TeardownHang) + Send + Sync + 'static) {
    let _ = HANG_HOOK.set(Box::new(hook));
}

/// Number of native teardowns that are hung now.
pub fn hung_teardowns() -> usize {
    HUNG_TEARDOWNS.load(Ordering::Acquire)
}

/// Step recorder handed to a teardown job.
pub struct TeardownSteps {
    label: &'static str,
    started: Instant,
    shared: Arc<Shared>,
}

impl TeardownSteps {
    /// Record and log the start of a step. The watchdog reports the last step
    /// that started.
    pub fn step(&self, name: &'static str) {
        if let Ok(mut current) = self.shared.step.lock() {
            *current = name;
        }
        log::info!(
            "teardown[{}]: {} (+{} ms)",
            self.label,
            name,
            self.started.elapsed().as_millis()
        );
    }
}

struct Shared {
    done: Mutex<bool>,
    done_cv: Condvar,
    step: Mutex<&'static str>,
    hung: AtomicBool,
}

/// Handle to a running teardown. Dropping it does not wait.
pub struct TeardownHandle {
    shared: Arc<Shared>,
}

impl TeardownHandle {
    /// Wait for the teardown to finish, at most `timeout`. Returns true when it
    /// finished. Intended for tests and for process exit, never for the command
    /// loop.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let Ok(done) = self.shared.done.lock() else {
            return false;
        };
        match self
            .shared
            .done_cv
            .wait_timeout_while(done, timeout, |finished| !*finished)
        {
            Ok((finished, _)) => *finished,
            Err(_) => false,
        }
    }
}

/// Run `job` on a dedicated thread with the default deadline.
pub fn spawn(
    label: &'static str,
    job: impl FnOnce(&TeardownSteps) + Send + 'static,
) -> TeardownHandle {
    spawn_with_deadline(label, TEARDOWN_DEADLINE, job)
}

/// Run `job` on a dedicated thread. A watchdog reports it if it runs longer
/// than `deadline`. The caller never waits.
pub fn spawn_with_deadline(
    label: &'static str,
    deadline: Duration,
    job: impl FnOnce(&TeardownSteps) + Send + 'static,
) -> TeardownHandle {
    let shared = Arc::new(Shared {
        done: Mutex::new(false),
        done_cv: Condvar::new(),
        step: Mutex::new("start"),
        hung: AtomicBool::new(false),
    });
    let started = Instant::now();

    let worker_shared = Arc::clone(&shared);
    let worker = std::thread::Builder::new()
        .name(format!("mello-teardown-{label}"))
        .spawn(move || {
            let steps = TeardownSteps {
                label,
                started,
                shared: Arc::clone(&worker_shared),
            };
            steps.step("start");
            job(&steps);
            finish(label, started, &worker_shared);
        });

    match worker {
        Ok(_) => {
            let watchdog_shared = Arc::clone(&shared);
            let watchdog = std::thread::Builder::new()
                .name(format!("mello-teardown-watchdog-{label}"))
                .spawn(move || watch(label, started, deadline, &watchdog_shared));
            if let Err(e) = watchdog {
                log::warn!(
                    "teardown[{}]: watchdog thread failed to start: {}",
                    label,
                    e
                );
            }
        }
        Err(e) => {
            // No thread means the job closure was dropped unrun. Its captured
            // resources are released here, which is only safe because the
            // native object was never stopped either: report it loudly.
            log::error!(
                "teardown[{}]: worker thread failed to start: {}. Native object not stopped.",
                label,
                e
            );
            finish(label, started, &shared);
        }
    }

    TeardownHandle { shared }
}

fn finish(label: &'static str, started: Instant, shared: &Shared) {
    // `done` and the `hung` read happen under the same lock the watchdog holds
    // while it marks a hang, so a teardown is either reported and decremented,
    // or neither.
    let was_hung = match shared.done.lock() {
        Ok(mut done) => {
            *done = true;
            shared.hung.load(Ordering::Acquire)
        }
        Err(_) => shared.hung.load(Ordering::Acquire),
    };
    shared.done_cv.notify_all();
    if was_hung {
        let remaining = HUNG_TEARDOWNS
            .fetch_sub(1, Ordering::AcqRel)
            .saturating_sub(1);
        log::warn!(
            "teardown[{}]: finished after {} ms, past its deadline (hung now: {})",
            label,
            started.elapsed().as_millis(),
            remaining
        );
    } else {
        log::info!(
            "teardown[{}]: done in {} ms",
            label,
            started.elapsed().as_millis()
        );
    }
}

fn watch(label: &'static str, started: Instant, deadline: Duration, shared: &Shared) {
    let Ok(done) = shared.done.lock() else {
        return;
    };
    let Ok((done, _)) = shared
        .done_cv
        .wait_timeout_while(done, deadline, |finished| !*finished)
    else {
        return;
    };
    if *done {
        return;
    }
    // Still holding the `done` lock: `finish` cannot run between this check
    // and the mark below.
    shared.hung.store(true, Ordering::Release);
    let hung_total = HUNG_TEARDOWNS.fetch_add(1, Ordering::AcqRel) + 1;
    drop(done);
    let step = shared.step.lock().map(|s| *s).unwrap_or("unknown");
    let hang = TeardownHang {
        label,
        step,
        elapsed: started.elapsed(),
        hung_total,
    };
    log::error!(
        "teardown[{}]: HUNG in step '{}' after {} ms (hung now: {}). Resources stay allocated.",
        hang.label,
        hang.step,
        hang.elapsed.as_millis(),
        hang.hung_total
    );
    if let Some(hook) = HANG_HOOK.get() {
        hook(&hang);
    }
}

type TeardownJob = Box<dyn FnOnce(&TeardownSteps) + Send>;

/// Owns native resources and tears them down on a dedicated thread when
/// dropped. Construct it as soon as a native object exists, so every exit path
/// (normal stop, error, task abort, panic) tears it down the same bounded way.
pub struct NativeTeardownGuard {
    label: &'static str,
    job: Option<TeardownJob>,
}

impl NativeTeardownGuard {
    /// Wrap a teardown job. The job runs once, when the guard drops.
    pub fn new(label: &'static str, job: impl FnOnce(&TeardownSteps) + Send + 'static) -> Self {
        Self {
            label,
            job: Some(Box::new(job)),
        }
    }

    /// Drop the guard now and return the handle to the teardown it starts.
    pub fn teardown(mut self) -> Option<TeardownHandle> {
        self.job.take().map(|job| spawn(self.label, job))
    }
}

impl Drop for NativeTeardownGuard {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            spawn(self.label, job);
        }
    }
}

/// Raw pointer that may move to the teardown thread.
///
/// libmello objects are safe to stop from any thread; the invariant the owner
/// keeps is that nothing else uses the pointer after it moves here.
pub struct TeardownPtr<T>(pub *mut T);

// SAFETY: see the type documentation. The pointer is used by exactly one thread
// after the move.
unsafe impl<T> Send for TeardownPtr<T> {}

impl<T> TeardownPtr<T> {
    /// The wrapped pointer.
    pub fn get(&self) -> *mut T {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn guard_drop_returns_while_native_stop_blocks() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (entered_tx, entered_rx) = mpsc::channel::<()>();

        let guard = NativeTeardownGuard::new("test_block", move |steps| {
            steps.step("blocking native stop");
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        });

        let before = Instant::now();
        let handle = guard.teardown().expect("job present");
        assert!(
            before.elapsed() < Duration::from_millis(500),
            "dropping the guard must not wait for the native stop"
        );
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("job started");
        assert!(!handle.wait_timeout(Duration::from_millis(50)));

        release_tx.send(()).expect("release");
        assert!(handle.wait_timeout(Duration::from_secs(5)));
    }

    #[test]
    fn watchdog_reports_the_step_that_hung() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (hang_tx, hang_rx) = mpsc::channel::<TeardownHang>();
        let hang_tx = Mutex::new(hang_tx);

        // The hook is process-wide; filter on this test's label.
        set_hang_hook(move |hang| {
            if hang.label == "test_hang" {
                if let Ok(tx) = hang_tx.lock() {
                    let _ = tx.send(hang.clone());
                }
            }
        });

        let handle = spawn_with_deadline("test_hang", Duration::from_millis(50), move |steps| {
            steps.step("capture stop");
            let _ = release_rx.recv();
        });

        match hang_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(hang) => {
                assert_eq!(hang.step, "capture stop");
                assert!(hang.hung_total >= 1);
            }
            Err(_) => {
                // Another test installed the hook first. The counter still
                // proves the watchdog fired.
                let start = Instant::now();
                while hung_teardowns() == 0 && start.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(hung_teardowns() >= 1);
            }
        }

        release_tx.send(()).expect("release");
        assert!(handle.wait_timeout(Duration::from_secs(5)));
    }

    #[test]
    fn teardown_within_deadline_is_not_reported() {
        let handle = spawn_with_deadline("test_fast", Duration::from_secs(5), |steps| {
            steps.step("quick stop");
        });
        assert!(handle.wait_timeout(Duration::from_secs(5)));
        assert!(!handle.shared.hung.load(Ordering::Acquire));
    }
}
