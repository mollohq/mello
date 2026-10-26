//! Coalesces the server's "feed changed" pushes (Nakama notification code 116)
//! into at most one feed reload every five seconds.
//!
//! A burst of session ends in a busy crew would otherwise load the feed once
//! for each. The first change loads at once. Changes inside the window set one
//! pending reload, which runs when the window ends, so the last change is never
//! lost. A change for a crew that is not the active one is ignored: the feed of
//! that crew loads when the user opens it.

use std::time::{Duration, Instant};

/// The shortest time between two reloads caused by pushes.
pub const FEED_RELOAD_WINDOW: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub struct FeedReloadGate {
    last_reload: Option<Instant>,
    pending: bool,
}

impl FeedReloadGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// A push arrived for `crew_id`. True means: load the feed of the active
    /// crew now.
    pub fn changed(&mut self, crew_id: &str, active_crew: Option<&str>, now: Instant) -> bool {
        if active_crew != Some(crew_id) {
            return false;
        }
        if self.window_open(now) {
            self.last_reload = Some(now);
            self.pending = false;
            true
        } else {
            self.pending = true;
            false
        }
    }

    /// True while a coalesced reload waits for the window to end.
    pub fn has_pending(&self) -> bool {
        self.pending
    }

    /// Called on a timer. True means: the pending reload is due, load the feed
    /// of the active crew now.
    pub fn due(&mut self, now: Instant) -> bool {
        if self.pending && self.window_open(now) {
            self.last_reload = Some(now);
            self.pending = false;
            return true;
        }
        false
    }

    /// The active crew changed or the user signed out: a pending reload is for
    /// a feed that is no longer on screen.
    pub fn clear(&mut self) {
        self.pending = false;
    }

    fn window_open(&self, now: Instant) -> bool {
        self.last_reload
            .is_none_or(|t| now.saturating_duration_since(t) >= FEED_RELOAD_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn the_first_change_reloads_at_once() {
        let mut g = FeedReloadGate::new();
        assert!(g.changed("a", Some("a"), Instant::now()));
    }

    #[test]
    fn a_burst_makes_one_reload_and_one_trailing_reload() {
        let mut g = FeedReloadGate::new();
        let t0 = Instant::now();
        assert!(g.changed("a", Some("a"), t0));
        for i in 1..20 {
            assert!(
                !g.changed("a", Some("a"), t0 + S * i / 10),
                "inside the window"
            );
        }
        assert!(g.has_pending());
        assert!(!g.due(t0 + S * 4), "window still open");
        assert!(g.due(t0 + S * 5), "trailing reload at the window end");
        assert!(!g.due(t0 + S * 6), "and only one");
        assert!(!g.has_pending());
    }

    #[test]
    fn at_most_one_reload_in_five_seconds() {
        let mut g = FeedReloadGate::new();
        let t0 = Instant::now();
        let mut reloads = 0;
        for ms in (0..12_000).step_by(100) {
            let now = t0 + Duration::from_millis(ms);
            if g.changed("a", Some("a"), now) || g.due(now) {
                reloads += 1;
            }
        }
        assert_eq!(reloads, 3, "at 0 s, 5 s and 10 s");
    }

    #[test]
    fn a_change_in_another_crew_does_nothing() {
        let mut g = FeedReloadGate::new();
        let t0 = Instant::now();
        assert!(!g.changed("b", Some("a"), t0));
        assert!(!g.has_pending());
        assert!(!g.changed("b", None, t0));
    }

    #[test]
    fn clear_drops_a_pending_reload() {
        let mut g = FeedReloadGate::new();
        let t0 = Instant::now();
        assert!(g.changed("a", Some("a"), t0));
        assert!(!g.changed("a", Some("a"), t0 + S));
        g.clear();
        assert!(!g.due(t0 + S * 6));
    }
}
