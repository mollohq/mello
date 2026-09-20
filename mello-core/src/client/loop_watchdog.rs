//! Watchdog for the core command loop.
//!
//! The command loop runs every command and the 20 ms voice tick on one task. A
//! command that awaits something slow stops all of it. On 2026-09-15 a native
//! stream stop never returned, and nothing in the log said which command held
//! the loop. Timing a command after it returns cannot catch that, so the
//! watchdog runs on its own thread and reports the command that is still
//! running.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A command running longer than this is logged as slow.
pub const SLOW_COMMAND: Duration = Duration::from_millis(500);
/// A command running longer than this is logged as blocking the loop.
pub const BLOCKED_COMMAND: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Current {
    name: Option<&'static str>,
    started: Option<Instant>,
    reported_blocked: bool,
}

/// Records which loop step runs now; a background thread reports long ones.
pub struct LoopWatchdog {
    current: Arc<Mutex<Current>>,
    stop: Arc<AtomicBool>,
}

/// Clears the current step when dropped, and logs a slow step.
pub struct LoopStepGuard<'a> {
    watchdog: &'a LoopWatchdog,
}

impl LoopWatchdog {
    /// Start the watchdog thread.
    pub fn start() -> Self {
        let current = Arc::new(Mutex::new(Current::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_current = Arc::clone(&current);
        let thread_stop = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("mello-loop-watchdog".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    std::thread::sleep(POLL);
                    check(&thread_current, BLOCKED_COMMAND);
                }
            });
        if let Err(e) = spawned {
            log::warn!("loop watchdog thread failed to start: {}", e);
        }
        Self { current, stop }
    }

    /// Mark the start of a loop step. The returned guard marks its end.
    pub fn step(&self, name: &'static str) -> LoopStepGuard<'_> {
        if let Ok(mut c) = self.current.lock() {
            c.name = Some(name);
            c.started = Some(Instant::now());
            c.reported_blocked = false;
        }
        LoopStepGuard { watchdog: self }
    }
}

impl Drop for LoopWatchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for LoopStepGuard<'_> {
    fn drop(&mut self) {
        let Ok(mut c) = self.watchdog.current.lock() else {
            return;
        };
        if let (Some(name), Some(started)) = (c.name.take(), c.started.take()) {
            let elapsed = started.elapsed();
            if c.reported_blocked {
                log::warn!(
                    "loop watchdog: '{}' released the command loop after {} ms",
                    name,
                    elapsed.as_millis()
                );
            } else if elapsed >= SLOW_COMMAND {
                log::warn!(
                    "loop watchdog: '{}' held the command loop for {} ms",
                    name,
                    elapsed.as_millis()
                );
            }
        }
        c.reported_blocked = false;
    }
}

/// Report the running step once when it passes `blocked_after`. Returns the
/// step name when it reported.
fn check(current: &Mutex<Current>, blocked_after: Duration) -> Option<&'static str> {
    let mut c = current.lock().ok()?;
    let (name, started) = (c.name?, c.started?);
    if c.reported_blocked || started.elapsed() < blocked_after {
        return None;
    }
    c.reported_blocked = true;
    log::error!(
        "loop watchdog: '{}' has blocked the command loop for {} ms (voice tick and commands are stalled)",
        name,
        started.elapsed().as_millis()
    );
    Some(name)
}

/// Stable name of a command for logs. Uses the serde tag, so it carries no
/// command payload (payloads can hold tokens).
pub fn command_name(cmd: &crate::command::Command) -> &'static str {
    let tag = serde_json::to_value(cmd).ok().and_then(|v| {
        v.get("type")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
    });
    match tag {
        Some(t) => intern(t),
        None => "unknown_command",
    }
}

/// Command names are a small closed set; keep one static copy of each.
fn intern(name: String) -> &'static str {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let set = NAMES.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut set) = set.lock() else {
        return "unknown_command";
    };
    if let Some(existing) = set.get(name.as_str()) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.into_boxed_str());
    set.insert(leaked);
    leaked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_step_that_is_still_running() {
        let current = Mutex::new(Current {
            name: Some("StopStream"),
            started: Some(Instant::now() - Duration::from_secs(3)),
            reported_blocked: false,
        });
        assert_eq!(check(&current, BLOCKED_COMMAND), Some("StopStream"));
        // Reported once, not on every poll.
        assert_eq!(check(&current, BLOCKED_COMMAND), None);
    }

    #[test]
    fn does_not_report_a_short_step() {
        let current = Mutex::new(Current {
            name: Some("VoiceSpeaking"),
            started: Some(Instant::now()),
            reported_blocked: false,
        });
        assert_eq!(check(&current, BLOCKED_COMMAND), None);
    }

    #[test]
    fn guard_clears_the_step() {
        let wd = LoopWatchdog::start();
        {
            let _g = wd.step("JoinVoice");
            assert_eq!(wd.current.lock().expect("lock").name, Some("JoinVoice"));
        }
        assert_eq!(wd.current.lock().expect("lock").name, None);
    }

    #[test]
    fn command_name_uses_the_tag_without_payload() {
        let cmd = crate::command::Command::DeviceAuth {
            device_id: "secret-device".into(),
        };
        assert_eq!(command_name(&cmd), "DeviceAuth");
    }
}
