//! Speaking state: the in-band message on the SFU voice control channel and
//! the rate limit it shares with the Nakama presence RPC.
//!
//! The local VAD opens and closes on the libmello capture thread. In an SFU
//! call the change goes from there straight to the control channel
//! (`{"type":"speaking","on":true}`), so a remote speaking ring needs one SFU
//! hop and never waits on the command loop (spec 10 section 7.4, mello-sfu
//! 01-SFU.md section 5.3).

use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::transport::AudioSendTarget;

/// At most one in-band speaking change per this interval (01-SFU.md §5.3).
pub const INBAND_MIN_CHANGE_INTERVAL: Duration = Duration::from_millis(100);

/// At most one `voice_speaking` presence RPC per this interval while in an
/// SFU call. The RPC only serves crew members outside the channel.
pub const PRESENCE_MIN_CHANGE_INTERVAL: Duration = Duration::from_secs(1);

/// Sends at most one change per interval. A change inside the interval waits
/// and [`ChangeLimiter::poll`] sends it when the interval ends, so the last
/// state always goes out and a remote ring never sticks. A change back to
/// the state already sent cancels the wait.
#[derive(Debug, Clone)]
pub struct ChangeLimiter {
    interval: Duration,
    last_sent_at: Option<Instant>,
    last_sent: Option<bool>,
    pending: Option<bool>,
    latest: Option<bool>,
}

impl ChangeLimiter {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_sent_at: None,
            last_sent: None,
            pending: None,
            latest: None,
        }
    }

    /// A new state. Returns the state to send now, if any.
    pub fn on_change(&mut self, state: bool, now: Instant) -> Option<bool> {
        self.latest = Some(state);
        self.pending = Some(state);
        self.poll(now)
    }

    /// The waiting state, once its interval has passed.
    pub fn poll(&mut self, now: Instant) -> Option<bool> {
        let state = self.pending?;
        if self.last_sent == Some(state) {
            self.pending = None;
            return None;
        }
        if let Some(at) = self.last_sent_at {
            if now.saturating_duration_since(at) < self.interval {
                return None;
            }
        }
        self.pending = None;
        self.last_sent = Some(state);
        self.last_sent_at = Some(now);
        Some(state)
    }

    /// The send of `state` failed: keep it waiting, without a new interval.
    pub fn send_failed(&mut self, state: bool) {
        if self.last_sent == Some(state) {
            self.last_sent = None;
            self.last_sent_at = None;
        }
        if self.pending.is_none() {
            self.pending = Some(state);
        }
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// A new receiver: nothing was sent to it yet. A speaking state that is
    /// on now goes out at the next poll.
    pub fn restart(&mut self) {
        self.last_sent = None;
        self.last_sent_at = None;
        self.pending = match self.latest {
            Some(true) => Some(true),
            _ => None,
        };
    }
}

fn speaking_message(on: bool) -> &'static str {
    if on {
        r#"{"type":"speaking","on":true}"#
    } else {
        r#"{"type":"speaking","on":false}"#
    }
}

/// The local speaking state as in-band control messages to the SFU.
///
/// [`InbandSpeaking::on_vad`] runs on the libmello capture thread (the VAD
/// callback); [`InbandSpeaking::poll`] runs on the voice tick and sends a
/// change that waited for its interval. The target is set while in an SFU
/// call; without it, changes are only recorded.
pub struct InbandSpeaking {
    target: RwLock<Option<Arc<AudioSendTarget>>>,
    limiter: Mutex<ChangeLimiter>,
}

impl Default for InbandSpeaking {
    fn default() -> Self {
        Self::new()
    }
}

impl InbandSpeaking {
    pub fn new() -> Self {
        Self {
            target: RwLock::new(None),
            limiter: Mutex::new(ChangeLimiter::new(INBAND_MIN_CHANGE_INTERVAL)),
        }
    }

    /// Send changes to `target` from now on.
    pub fn attach(&self, target: Arc<AudioSendTarget>) {
        if let Ok(mut t) = self.target.write() {
            *t = Some(target);
        }
        if let Ok(mut l) = self.limiter.lock() {
            l.restart();
        }
        self.poll();
    }

    /// Stop sending. Waits for a send that runs now.
    pub fn detach(&self) {
        if let Ok(mut t) = self.target.write() {
            *t = None;
        }
    }

    /// The VAD opened or closed.
    pub fn on_vad(&self, speaking: bool) {
        let send = match self.limiter.lock() {
            Ok(mut l) => l.on_change(speaking, Instant::now()),
            Err(_) => None,
        };
        if let Some(on) = send {
            self.send(on);
        }
    }

    /// Send a change that waited for its interval.
    pub fn poll(&self) {
        let send = match self.limiter.lock() {
            Ok(mut l) if l.has_pending() => l.poll(Instant::now()),
            _ => None,
        };
        if let Some(on) = send {
            self.send(on);
        }
    }

    fn send(&self, on: bool) {
        let sent = match self.target.read() {
            Ok(t) => t
                .as_ref()
                .is_some_and(|target| target.send_control_text(speaking_message(on))),
            Err(_) => false,
        };
        if sent {
            log::debug!("SFU voice: in-band speaking={}", on);
        } else if let Ok(mut l) = self.limiter.lock() {
            l.send_failed(on);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_first_change_goes_out_at_once() {
        let t0 = Instant::now();
        let mut l = ChangeLimiter::new(100 * MS);
        assert_eq!(l.on_change(true, t0), Some(true));
        assert!(!l.has_pending());
    }

    #[test]
    fn a_change_inside_the_interval_waits_and_is_not_lost() {
        let t0 = Instant::now();
        let mut l = ChangeLimiter::new(100 * MS);
        assert_eq!(l.on_change(true, t0), Some(true));
        assert_eq!(l.on_change(false, t0 + 30 * MS), None);
        assert!(l.has_pending());
        assert_eq!(l.poll(t0 + 99 * MS), None);
        assert_eq!(l.poll(t0 + 100 * MS), Some(false));
        assert!(!l.has_pending());
    }

    #[test]
    fn a_flap_back_inside_the_interval_sends_nothing() {
        let t0 = Instant::now();
        let mut l = ChangeLimiter::new(100 * MS);
        l.on_change(true, t0);
        assert_eq!(l.on_change(false, t0 + 20 * MS), None);
        assert_eq!(l.on_change(true, t0 + 40 * MS), None);
        assert!(!l.has_pending(), "back at the sent state: nothing waits");
        assert_eq!(l.poll(t0 + 200 * MS), None);
    }

    #[test]
    fn at_most_one_change_per_interval() {
        let t0 = Instant::now();
        let mut l = ChangeLimiter::new(100 * MS);
        let mut sent = Vec::new();
        let mut state = false;
        for step in 0..100u32 {
            state = !state;
            let now = t0 + step * 10 * MS;
            if let Some(s) = l.on_change(state, now) {
                sent.push((now, s));
            }
        }
        for pair in sent.windows(2) {
            assert!(pair[1].0 - pair[0].0 >= 100 * MS, "{:?}", pair);
        }
        assert!(
            sent.len() >= 9,
            "one per interval still goes out: {}",
            sent.len()
        );
    }

    #[test]
    fn a_failed_send_is_retried() {
        let t0 = Instant::now();
        let mut l = ChangeLimiter::new(100 * MS);
        assert_eq!(l.on_change(true, t0), Some(true));
        l.send_failed(true);
        assert_eq!(l.poll(t0 + MS), Some(true), "no interval after a failure");
    }

    #[test]
    fn restart_resends_a_speaking_state_that_is_on() {
        let t0 = Instant::now();
        let mut l = ChangeLimiter::new(100 * MS);
        l.on_change(true, t0);
        l.restart();
        assert_eq!(l.poll(t0 + MS), Some(true));

        let mut quiet = ChangeLimiter::new(100 * MS);
        quiet.on_change(false, t0);
        quiet.restart();
        assert!(!quiet.has_pending(), "silence needs no message");
    }

    #[test]
    fn without_a_target_the_state_waits_for_attach() {
        let s = InbandSpeaking::new();
        s.on_vad(true); // no target: kept
        assert!(s.limiter.lock().unwrap().has_pending());
    }
}
