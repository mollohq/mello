//! Stream pause UX: host tab-out detection, control message, viewer parsing.
//!
//! When the streamer minimizes or tabs out of the captured game, viewers see
//! a pause card instead of a frozen frame. The host owns the state; viewers
//! only render what the control channel tells them.
//!
//! Wire format (reliable control channel, same family as the cursor packet
//! `0x04/0x02`): `[0x04, 0x03, state]`, state 1 = paused, 0 = live.

/// Control-message type shared with the cursor channel.
pub const PAUSE_MESSAGE_TYPE: u8 = 0x04;
/// Control-message subtype for stream pause.
pub const PAUSE_MESSAGE_SUBTYPE: u8 = 0x03;

/// Consecutive 1 Hz manager ticks with an unavailable capture target before
/// viewers are told the stream paused. Hides quick ALT-Tabs; a real tab-out
/// still shows the card in ~3 s.
pub const PAUSE_ENTER_UNAVAILABLE_TICKS: u32 = 3;

/// Encode the pause state for the reliable control channel.
pub fn pause_message(paused: bool) -> [u8; 3] {
    [PAUSE_MESSAGE_TYPE, PAUSE_MESSAGE_SUBTYPE, u8::from(paused)]
}

/// Parse a control-channel datagram. Returns the pause state, or `None` when
/// the datagram is some other control message (cursor, ping payload, ...).
/// Never fails: unknown input is not a pause message, not an error.
pub fn parse_pause_message(data: &[u8]) -> Option<bool> {
    match data {
        [t, s, state] if *t == PAUSE_MESSAGE_TYPE && *s == PAUSE_MESSAGE_SUBTYPE => {
            Some(*state != 0)
        }
        _ => None,
    }
}

/// Host-side pause state machine. Fed the polled capture-target availability
/// (~1 Hz from `MelloStreamStats::target_available`); emits the new state on
/// transitions only.
///
/// Entering is debounced (quick tab-outs never reach viewers); leaving is
/// immediate (tab-back resumes at once).
#[derive(Debug, Default)]
pub struct PauseController {
    unavailable_ticks: u32,
    paused: bool,
}

impl PauseController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current state (what viewers were last told).
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Feed one availability sample. Returns `Some(new_state)` on a
    /// transition, `None` when nothing changed.
    pub fn observe(&mut self, target_available: bool) -> Option<bool> {
        if target_available {
            self.unavailable_ticks = 0;
            if self.paused {
                self.paused = false;
                return Some(false);
            }
            return None;
        }
        self.unavailable_ticks = self.unavailable_ticks.saturating_add(1);
        if !self.paused && self.unavailable_ticks >= PAUSE_ENTER_UNAVAILABLE_TICKS {
            self.paused = true;
            return Some(true);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_message_round_trips() {
        assert_eq!(parse_pause_message(&pause_message(true)), Some(true));
        assert_eq!(parse_pause_message(&pause_message(false)), Some(false));
    }

    #[test]
    fn non_pause_control_messages_are_ignored() {
        assert_eq!(parse_pause_message(&[]), None);
        assert_eq!(parse_pause_message(&[0x04, 0x02, 0x10]), None);
        assert_eq!(parse_pause_message(&[0x04, 0x03]), None);
        assert_eq!(parse_pause_message(&[0x04, 0x03, 1, 0x99]), None);
        assert_eq!(parse_pause_message(b"ping"), None);
    }

    #[test]
    fn brief_tab_out_never_pauses() {
        let mut c = PauseController::new();
        assert_eq!(c.observe(false), None);
        assert_eq!(c.observe(false), None);
        assert_eq!(c.observe(true), None);
        assert!(!c.is_paused());
    }

    #[test]
    fn sustained_unavailable_pauses_after_three_ticks() {
        let mut c = PauseController::new();
        assert_eq!(c.observe(false), None);
        assert_eq!(c.observe(false), None);
        assert_eq!(c.observe(false), Some(true));
        assert!(c.is_paused());
        // Stays paused without re-emitting.
        assert_eq!(c.observe(false), None);
    }

    #[test]
    fn tab_back_resumes_immediately() {
        let mut c = PauseController::new();
        for _ in 0..PAUSE_ENTER_UNAVAILABLE_TICKS {
            let _ = c.observe(false);
        }
        assert!(c.is_paused());
        assert_eq!(c.observe(true), Some(false));
        assert!(!c.is_paused());
        assert_eq!(c.observe(true), None);
    }
}
