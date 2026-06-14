//! Pure decision logic for the realtime-WebSocket reconnect supervisor.
//!
//! All timing/state decisions live here as a side-effect-free state machine so
//! they can be unit-tested deterministically with injected clocks, while
//! `connection_tick` stays a thin adapter that performs the actual IO
//! (force-disconnect, connect, emit events, heartbeat) for each decision.

use std::time::{Duration, SystemTime};
use tokio::time::Instant;

/// A gap larger than this between consecutive connection ticks (scheduled every
/// 3s) means the process was suspended (laptop slept). On wake, live sockets are
/// usually half-open, so we proactively rebuild them.
pub(crate) const WAKE_GAP: Duration = Duration::from_secs(30);
/// Cap for the realtime WS reconnect backoff.
pub(crate) const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);
/// How often to refresh presence while connected.
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// What the adapter should do this tick. Multiple fields can be set in one tick
/// (e.g. a wake forces a disconnect, emits the "lost" banner, and triggers an
/// immediate reconnect attempt).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    /// Machine resumed from suspend: drop + rebuild sockets and re-establish voice.
    pub woke_from_sleep: bool,
    /// WS transitioned up -> down this tick: surface the "reconnecting" banner.
    pub connection_lost_edge: bool,
    /// Connected and a presence heartbeat is due.
    pub heartbeat_due: bool,
    /// Disconnected and the backoff timer says it's time to try reconnecting.
    pub attempt_reconnect: bool,
}

/// Side-effect-free reconnect/liveness state machine.
#[derive(Debug, Default)]
pub(crate) struct ReconnectSupervisor {
    ws_alive: bool,
    attempt: u32,
    next_at: Option<Instant>,
    last_liveness_check: Option<SystemTime>,
    last_heartbeat: Option<Instant>,
}

impl ReconnectSupervisor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Decide what to do this tick given the sampled clock and socket state.
    /// Mutates only the state that does NOT depend on async IO outcomes; the
    /// reconnect attempt's success/failure is reported back via
    /// [`begin_reconnect_attempt`] + [`record_reconnect_result`].
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        wall_now: SystemTime,
        connected: bool,
        has_session: bool,
    ) -> Decision {
        let mut d = Decision::default();

        let woke = self
            .last_liveness_check
            .and_then(|prev| wall_now.duration_since(prev).ok())
            .map(|gap| gap > WAKE_GAP)
            .unwrap_or(false);
        self.last_liveness_check = Some(wall_now);

        // Not authenticated: nothing to supervise, leave reconnect state untouched.
        if !has_session {
            return d;
        }

        // On wake we will force the (likely half-open) socket down, so treat it
        // as disconnected for the rest of this decision and schedule an
        // immediate reconnect.
        let mut connected = connected;
        if woke {
            d.woke_from_sleep = true;
            connected = false;
            self.next_at = Some(now);
        }

        if self.ws_alive && !connected {
            d.connection_lost_edge = true;
            if self.next_at.is_none() {
                self.next_at = Some(now);
            }
        }
        self.ws_alive = connected;

        if connected {
            self.attempt = 0;
            self.next_at = None;
            if self.heartbeat_due(now) {
                d.heartbeat_due = true;
            }
            return d;
        }

        let due = self.next_at.map(|at| now >= at).unwrap_or(true);
        if due {
            d.attempt_reconnect = true;
        }
        d
    }

    fn heartbeat_due(&self, now: Instant) -> bool {
        self.last_heartbeat
            .map(|t| now.duration_since(t) >= HEARTBEAT_INTERVAL)
            .unwrap_or(true)
    }

    /// Record that a heartbeat was just sent.
    pub(crate) fn record_heartbeat(&mut self, now: Instant) {
        self.last_heartbeat = Some(now);
    }

    /// Increment + return the current attempt number. Call right before the
    /// async connect, mirroring the original (attempt counts the in-flight try).
    pub(crate) fn begin_reconnect_attempt(&mut self) -> u32 {
        self.attempt += 1;
        self.attempt
    }

    /// Feed back the connect outcome: reset on success, schedule capped
    /// exponential backoff on failure.
    pub(crate) fn record_reconnect_result(&mut self, now: Instant, ok: bool) {
        if ok {
            self.attempt = 0;
            self.next_at = None;
            self.ws_alive = true;
        } else {
            self.next_at = Some(now + self.backoff());
        }
    }

    fn backoff(&self) -> Duration {
        let secs = 2u64
            .saturating_pow(self.attempt.min(5))
            .min(MAX_RECONNECT_BACKOFF.as_secs());
        Duration::from_secs(secs)
    }

    /// Backdate the liveness anchor so the next `poll` detects a wake gap. Used
    /// by the `test-faults` suspend simulator.
    #[cfg(feature = "test-faults")]
    pub(crate) fn backdate_liveness(&mut self) {
        self.last_liveness_check =
            SystemTime::now().checked_sub(WAKE_GAP + Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    // A fresh supervisor with a live socket should do nothing notable, then a
    // due heartbeat fires once and resets until the interval elapses again.
    #[test]
    fn connected_steady_state_heartbeats_on_interval() {
        let mut s = ReconnectSupervisor::new();
        let t0 = base();
        let w0 = SystemTime::now();

        // First connected poll: heartbeat is due (never sent).
        let d = s.poll(t0, w0, true, true);
        assert!(d.heartbeat_due);
        assert!(!d.attempt_reconnect && !d.connection_lost_edge && !d.woke_from_sleep);
        s.record_heartbeat(t0);

        // Shortly after: not due yet.
        let d = s.poll(t0 + Duration::from_secs(5), w0, true, true);
        assert!(!d.heartbeat_due);

        // After the interval: due again.
        let d = s.poll(t0 + HEARTBEAT_INTERVAL, w0, true, true);
        assert!(d.heartbeat_due);
    }

    // No session => supervisor stays inert (no reconnect churn pre-login).
    #[test]
    fn no_session_is_inert() {
        let mut s = ReconnectSupervisor::new();
        let d = s.poll(base(), SystemTime::now(), false, false);
        assert_eq!(d, Decision::default());
    }

    // WS up -> down should raise the lost edge exactly once and then schedule a
    // reconnect attempt on the following ticks.
    #[test]
    fn lost_edge_then_reconnect_due() {
        let mut s = ReconnectSupervisor::new();
        let t0 = base();
        let w = SystemTime::now();

        // Establish "alive".
        s.poll(t0, w, true, true);

        // Drop: edge fires, attempt becomes due immediately (next_at = now).
        let d = s.poll(t0 + Duration::from_secs(3), w, false, true);
        assert!(d.connection_lost_edge);
        assert!(d.attempt_reconnect);

        // Edge does not re-fire while still down.
        let d = s.poll(t0 + Duration::from_secs(6), w, false, true);
        assert!(!d.connection_lost_edge);
        assert!(d.attempt_reconnect);
    }

    // Failed attempts back off exponentially and cap at MAX_RECONNECT_BACKOFF.
    #[test]
    fn backoff_is_exponential_and_capped() {
        let mut s = ReconnectSupervisor::new();
        let mut t = base();
        let w = SystemTime::now();

        // Get into a disconnected, due state.
        s.poll(t, w, true, true);
        let d = s.poll(t + Duration::from_secs(3), w, false, true);
        assert!(d.attempt_reconnect);
        t += Duration::from_secs(3);

        // 2^attempt capped at MAX_RECONNECT_BACKOFF (30s): attempt 5 would be 32 -> 30.
        let expected = [2u64, 4, 8, 16, 30, 30, 30];
        for exp in expected {
            let attempt = s.begin_reconnect_attempt();
            s.record_reconnect_result(t, false);
            // Not yet due before the backoff elapses...
            let d = s.poll(
                t + Duration::from_secs(exp) - Duration::from_millis(1),
                w,
                false,
                true,
            );
            assert!(
                !d.attempt_reconnect,
                "attempt {} should not be due early",
                attempt
            );
            // ...due once it does.
            let d = s.poll(t + Duration::from_secs(exp), w, false, true);
            assert!(
                d.attempt_reconnect,
                "attempt {} should be due at {}s",
                attempt, exp
            );
            t += Duration::from_secs(exp);
        }
    }

    // A successful reconnect resets the attempt counter and clears backoff.
    #[test]
    fn success_resets_state() {
        let mut s = ReconnectSupervisor::new();
        let t = base();
        let w = SystemTime::now();
        s.poll(t, w, true, true);
        s.poll(t + Duration::from_secs(3), w, false, true);

        s.begin_reconnect_attempt();
        s.begin_reconnect_attempt(); // attempt = 2
        s.record_reconnect_result(t + Duration::from_secs(3), true);

        // Now reports connected-steady; a later failure should restart at 2s.
        let d = s.poll(t + Duration::from_secs(4), w, true, true);
        assert!(!d.attempt_reconnect);
        // Drop again and confirm backoff starts from the first step (2s).
        s.poll(t + Duration::from_secs(7), w, false, true);
        let attempt = s.begin_reconnect_attempt();
        assert_eq!(
            attempt, 1,
            "attempt counter should have reset after success"
        );
    }

    // A large wall-clock gap between ticks is detected as a wake-from-sleep,
    // which forces a disconnect + immediate reconnect even if `connected` was
    // still reported true by the (stale) socket.
    #[test]
    fn wake_gap_forces_reconnect() {
        let mut s = ReconnectSupervisor::new();
        let t = base();
        let w0 = SystemTime::now();

        // Establish alive.
        s.poll(t, w0, true, true);

        // Next tick is >WAKE_GAP later on the wall clock (laptop slept), but the
        // monotonic `now` barely advanced and the socket still claims connected.
        let w1 = w0 + WAKE_GAP + Duration::from_secs(5);
        let d = s.poll(t + Duration::from_secs(3), w1, true, true);
        assert!(d.woke_from_sleep);
        assert!(
            d.connection_lost_edge,
            "stale socket should be treated as lost"
        );
        assert!(
            d.attempt_reconnect,
            "wake should trigger an immediate reconnect"
        );
    }

    // A normal small gap must NOT be mistaken for a wake.
    #[test]
    fn small_gap_is_not_a_wake() {
        let mut s = ReconnectSupervisor::new();
        let t = base();
        let w0 = SystemTime::now();
        s.poll(t, w0, true, true);
        let d = s.poll(
            t + Duration::from_secs(3),
            w0 + Duration::from_secs(3),
            true,
            true,
        );
        assert!(!d.woke_from_sleep);
    }
}
