//! Stream pause UX: host tab-out detection, control message, viewer parsing.
//!
//! When the streamer minimizes or tabs out of the captured game, viewers see
//! a pause card instead of a frozen frame. The host owns the state; viewers
//! only render what the control channel tells them.
//!
//! Wire format (reliable control channel, same family as the cursor packet
//! `0x04/0x02`):
//!
//! * v1: `[0x04, 0x03, state]`, state 1 = paused, 0 = live.
//! * v2: `[0x04, 0x03, state, reason]`, with the capture state behind the
//!   pause. New hosts send both, so old viewers (v1 only) still get the
//!   card while new viewers get the reason.

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

/// Why the stream paused, for the viewer's pause card. A minimized game, one
/// that has drawn nothing yet, and one no method can see are three different
/// sentences; the capture layer already distinguishes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PauseReason {
    /// Live, or paused by an older host that sent no reason.
    #[default]
    Unknown = 0,
    /// The game is minimized or tabbed out.
    Minimized = 1,
    /// The game has drawn nothing yet (loading, launching).
    WaitingForGame = 2,
    /// No capture method can see the game.
    Failed = 3,
}

impl PauseReason {
    /// Map a capture state (`CAPTURE_STATE_*` in manager.rs) to card copy.
    /// The numbers are the C API's, not this enum's: keep the mapping next
    /// to the wire format so the two never drift.
    pub fn from_capture_state(state: u32) -> Self {
        match state {
            0 => PauseReason::Unknown,
            1 => PauseReason::Minimized,
            2 => PauseReason::WaitingForGame,
            3 => PauseReason::Failed,
            _ => PauseReason::Unknown,
        }
    }

    /// Short code for the UI layer and the log.
    pub fn as_str(self) -> &'static str {
        match self {
            PauseReason::Unknown => "unknown",
            PauseReason::Minimized => "minimized",
            PauseReason::WaitingForGame => "waiting",
            PauseReason::Failed => "failed",
        }
    }
}

/// Encode the pause state with the reason (v2).
pub fn pause_message_with_reason(paused: bool, reason: PauseReason) -> [u8; 4] {
    [
        PAUSE_MESSAGE_TYPE,
        PAUSE_MESSAGE_SUBTYPE,
        u8::from(paused),
        reason as u8,
    ]
}

/// Parse a control-channel datagram. Returns the pause state, or `None` when
/// the datagram is some other control message (cursor, ping payload, ...).
/// Never fails: unknown input is not a pause message, not an error.
pub fn parse_pause_message(data: &[u8]) -> Option<bool> {
    parse_pause_message_full(data).map(|(paused, _)| paused)
}

/// Parse a pause datagram with its reason. Accepts both v1 (reason Unknown)
/// and v2. Anything else is not a pause message.
pub fn parse_pause_message_full(data: &[u8]) -> Option<(bool, PauseReason)> {
    match data {
        [t, s, state] if *t == PAUSE_MESSAGE_TYPE && *s == PAUSE_MESSAGE_SUBTYPE => {
            Some((*state != 0, PauseReason::Unknown))
        }
        [t, s, state, reason] if *t == PAUSE_MESSAGE_TYPE && *s == PAUSE_MESSAGE_SUBTYPE => {
            let reason = match reason {
                1 => PauseReason::Minimized,
                2 => PauseReason::WaitingForGame,
                3 => PauseReason::Failed,
                _ => PauseReason::Unknown,
            };
            Some((*state != 0, reason))
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
    fn v2_reason_round_trips() {
        let msg = pause_message_with_reason(true, PauseReason::WaitingForGame);
        assert_eq!(
            parse_pause_message_full(&msg),
            Some((true, PauseReason::WaitingForGame))
        );
        // v1 compat: old hosts carry no reason.
        assert_eq!(
            parse_pause_message_full(&pause_message(true)),
            Some((true, PauseReason::Unknown))
        );
        // The state-only parser reads both versions; the reason rides
        // along only through the full parser.
        assert_eq!(parse_pause_message(&msg), Some(true));
    }

    #[test]
    fn unknown_reason_codes_stay_unknown() {
        assert_eq!(
            parse_pause_message_full(&[0x04, 0x03, 1, 0x99]),
            Some((true, PauseReason::Unknown))
        );
        assert_eq!(
            parse_pause_message_full(&[0x04, 0x03, 0, 0x02]),
            Some((false, PauseReason::WaitingForGame))
        );
    }

    #[test]
    fn capture_states_map_to_card_reasons() {
        assert_eq!(PauseReason::from_capture_state(1), PauseReason::Minimized);
        assert_eq!(
            PauseReason::from_capture_state(2),
            PauseReason::WaitingForGame
        );
        assert_eq!(PauseReason::from_capture_state(3), PauseReason::Failed);
        assert_eq!(PauseReason::from_capture_state(0), PauseReason::Unknown);
        assert_eq!(PauseReason::from_capture_state(99), PauseReason::Unknown);
    }

    #[test]
    fn non_pause_control_messages_are_ignored() {
        assert_eq!(parse_pause_message(&[]), None);
        assert_eq!(parse_pause_message(&[0x04, 0x02, 0x10]), None);
        assert_eq!(parse_pause_message(&[0x04, 0x03]), None);
        assert_eq!(parse_pause_message(&[0x04, 0x03, 1, 0x99, 0x00]), None);
        assert_eq!(parse_pause_message(b"ping"), None);
        // A v2 datagram with an unknown reason is still a pause message;
        // the reason just reads as Unknown (covered above).
        assert_eq!(parse_pause_message(&[0x04, 0x03, 1, 0x99]), Some(true));
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
