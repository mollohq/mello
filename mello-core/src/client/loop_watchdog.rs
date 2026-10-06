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

/// A voice tick in SFU mode longer than this is logged. In SFU mode no audio
/// waits for the tick (plans/voice-quality.md stage 2), so a long tick means
/// a step that does not belong on it.
pub const VOICE_TICK_BUDGET: Duration = Duration::from_millis(5);
/// At most one over-budget line per this interval; the rest are counted.
const VOICE_TICK_REPORT_INTERVAL: Duration = Duration::from_secs(10);

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

/// Times the steps of one voice tick. Each [`TickSteps::mark`] ends the step
/// that started at the previous mark.
pub struct TickSteps {
    started: Instant,
    last: Instant,
    slowest: Option<(&'static str, Duration)>,
}

impl TickSteps {
    pub fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            slowest: None,
        }
    }

    /// End the step `name`: the time since the previous mark.
    pub fn mark(&mut self, name: &'static str) {
        let now = Instant::now();
        self.record(name, now - self.last);
        self.last = now;
    }

    fn record(&mut self, name: &'static str, elapsed: Duration) {
        if self.slowest.is_none_or(|(_, d)| elapsed > d) {
            self.slowest = Some((name, elapsed));
        }
    }

    /// Time from the start to the last mark.
    pub fn total(&self) -> Duration {
        self.last - self.started
    }

    /// The step that took the longest, if any step was marked.
    pub fn slowest(&self) -> Option<(&'static str, Duration)> {
        self.slowest
    }
}

/// Reports a voice tick over [`VOICE_TICK_BUDGET`] in SFU mode, at warn
/// level, with its slowest step. Rate-limited: one line per 10 s, with the
/// count of the long ticks in between.
#[derive(Default)]
pub struct VoiceTickBudget {
    last_report: Option<Instant>,
    suppressed: u32,
}

impl VoiceTickBudget {
    /// Check one finished tick. Returns the line it logged, if any.
    pub fn check(&mut self, sfu_mode: bool, steps: &TickSteps, now: Instant) -> Option<String> {
        let total = steps.total();
        if !over_voice_tick_budget(sfu_mode, total) {
            return None;
        }
        if self
            .last_report
            .is_some_and(|at| now.duration_since(at) < VOICE_TICK_REPORT_INTERVAL)
        {
            self.suppressed += 1;
            return None;
        }
        let (step, step_time) = steps.slowest().unwrap_or(("unknown", total));
        let line = format!(
            "voice tick took {:.1} ms in SFU mode (budget {} ms); slowest step '{}' {:.1} ms; {} more long ticks since the last report",
            total.as_secs_f64() * 1000.0,
            VOICE_TICK_BUDGET.as_millis(),
            step,
            step_time.as_secs_f64() * 1000.0,
            self.suppressed
        );
        log::warn!("loop watchdog: {}", line);
        self.last_report = Some(now);
        self.suppressed = 0;
        Some(line)
    }
}

/// True when a voice tick of `elapsed` breaks the budget. Only SFU mode has
/// the budget: P2P and the mic test still send audio on the tick.
fn over_voice_tick_budget(sfu_mode: bool, elapsed: Duration) -> bool {
    sfu_mode && elapsed > VOICE_TICK_BUDGET
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

    fn steps_with(name: &'static str, elapsed: Duration) -> TickSteps {
        let started = Instant::now();
        let mut steps = TickSteps {
            started,
            last: started,
            slowest: None,
        };
        steps.record("mic_level", Duration::from_micros(10));
        steps.record(name, elapsed);
        steps.last = started + elapsed + Duration::from_micros(10);
        steps
    }

    #[test]
    fn voice_tick_budget_is_5_ms_in_sfu_mode_only() {
        assert!(!over_voice_tick_budget(true, Duration::from_micros(4_900)));
        assert!(!over_voice_tick_budget(true, VOICE_TICK_BUDGET));
        assert!(over_voice_tick_budget(true, Duration::from_micros(5_100)));
        // P2P and the mic test send audio on the tick: no budget.
        assert!(!over_voice_tick_budget(false, Duration::from_millis(50)));
    }

    #[test]
    fn a_long_sfu_tick_is_reported_with_its_slowest_step() {
        let mut budget = VoiceTickBudget::default();
        let now = Instant::now();
        let steps = steps_with("sfu_events", Duration::from_millis(12));
        let line = budget.check(true, &steps, now).expect("reported");
        assert!(line.contains("'sfu_events'"), "{line}");
        assert!(line.contains("12.0 ms"), "{line}");

        // A short tick and a long P2P tick are not reported.
        let mut quiet = VoiceTickBudget::default();
        assert_eq!(
            quiet.check(
                true,
                &steps_with("sfu_events", Duration::from_millis(1)),
                now
            ),
            None
        );
        assert_eq!(quiet.check(false, &steps, now), None);
    }

    #[test]
    fn long_ticks_are_rate_limited_and_counted() {
        let mut budget = VoiceTickBudget::default();
        let t0 = Instant::now();
        let steps = steps_with("sfu_liveness", Duration::from_millis(8));
        assert!(budget.check(true, &steps, t0).is_some());
        assert_eq!(
            budget.check(true, &steps, t0 + Duration::from_secs(1)),
            None
        );
        assert_eq!(
            budget.check(true, &steps, t0 + Duration::from_secs(2)),
            None
        );
        let line = budget
            .check(true, &steps, t0 + VOICE_TICK_REPORT_INTERVAL)
            .expect("reported after the interval");
        assert!(line.contains("2 more long ticks"), "{line}");
    }

    #[test]
    fn tick_steps_name_the_slowest_step() {
        let mut steps = TickSteps::start();
        steps.record("mic_level", Duration::from_micros(20));
        steps.record("sfu_liveness", Duration::from_millis(3));
        steps.record("sfu_events", Duration::from_micros(40));
        assert_eq!(
            steps.slowest(),
            Some(("sfu_liveness", Duration::from_millis(3)))
        );
    }

    #[test]
    fn command_name_uses_the_tag_without_payload() {
        let cmd = crate::command::Command::DeviceAuth {
            device_id: "secret-device".into(),
        };
        assert_eq!(command_name(&cmd), "DeviceAuth");
    }
}
