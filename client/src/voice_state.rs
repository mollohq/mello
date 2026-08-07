//! Local mute / deafen state.
//!
//! The user can toggle these from five places — the control bar, the tray menu,
//! the macOS menu bar, the HUD overlay and the Windows taskbar thumbnail — and
//! each had its own copy of the same fifteen-line state machine. The copies had
//! already drifted: deafening from the macOS menu bar left the tray's Mute
//! checkbox stale, because that copy alone omitted the update.
//!
//! The rules are small but easy to get subtly wrong:
//! - deafening implies muting;
//! - unmuting implies undeafening, so the user is never left talking into a
//!   call they cannot hear;
//! - undeafening restores the mic to whatever it was *before* deafening, so a
//!   user who muted deliberately stays muted;
//! - every change is broadcast to the crew.
//!
//! Together the first two make "deafened with a live mic" unrepresentable.

/// Mic and speaker state as the user sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuteState {
    pub muted: bool,
    pub deafened: bool,
    /// Mic state captured when deafening, restored on undeafen.
    pub muted_before_deafen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuteAction {
    ToggleMute,
    ToggleDeafen,
}

/// What the caller must do after a transition.
///
/// Returned rather than performed so the rules stay a pure function; the
/// client-side wrapper turns these into commands and property writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuteOutcome {
    pub state: MuteState,
    /// Send `Command::SetMute` when the mic actually changed.
    pub send_mute: Option<bool>,
    /// Send `Command::SetDeafen` when deafen actually changed.
    pub send_deafen: Option<bool>,
}

impl MuteState {
    #[must_use]
    pub fn apply(self, action: MuteAction) -> MuteOutcome {
        match action {
            MuteAction::ToggleMute => {
                let muted = !self.muted;
                // Opening the mic also undeafens. Otherwise the user ends up
                // talking into a call they cannot hear, with the deafen icon
                // still lit — which is never what they meant by unmuting.
                let deafened = self.deafened && muted;
                MuteOutcome {
                    state: MuteState {
                        muted,
                        deafened,
                        ..self
                    },
                    send_mute: Some(muted),
                    send_deafen: (deafened != self.deafened).then_some(deafened),
                }
            }

            MuteAction::ToggleDeafen => {
                let deafened = !self.deafened;
                if deafened {
                    // Remember the mic state so undeafening can restore it,
                    // then force the mic off.
                    let state = MuteState {
                        muted: true,
                        deafened: true,
                        muted_before_deafen: self.muted,
                    };
                    MuteOutcome {
                        state,
                        send_mute: (!self.muted).then_some(true),
                        send_deafen: Some(true),
                    }
                } else {
                    // Restore, rather than blindly unmuting: a user who muted
                    // deliberately before deafening expects to stay muted.
                    let muted = self.muted_before_deafen;
                    MuteOutcome {
                        state: MuteState {
                            muted,
                            deafened: false,
                            muted_before_deafen: self.muted_before_deafen,
                        },
                        send_mute: (muted != self.muted).then_some(muted),
                        send_deafen: Some(false),
                    }
                }
            }
        }
    }
}

/// Apply a mute/deafen action and perform every consequence.
///
/// The single place these effects happen. Previously each of the five entry
/// points repeated them, and they had already diverged — the macOS menu bar
/// forgot to refresh the tray checkbox.
pub fn dispatch(ctx: &crate::app_context::AppContext, action: MuteAction) {
    use mello_core::Command;

    let current = MuteState {
        muted: ctx.app.get_mic_muted(),
        deafened: ctx.app.get_deafened(),
        muted_before_deafen: ctx.muted_before_deafen.get(),
    };

    let outcome = current.apply(action);
    let next = outcome.state;

    ctx.app.set_mic_muted(next.muted);
    ctx.app.set_deafened(next.deafened);
    ctx.muted_before_deafen.set(next.muted_before_deafen);

    if let Some(deafened) = outcome.send_deafen {
        let _ = ctx.cmd_tx.send(Command::SetDeafen { deafened });
    }
    if let Some(muted) = outcome.send_mute {
        let _ = ctx.cmd_tx.send(Command::SetMute { muted });
    }

    let _ = ctx.cmd_tx.send(Command::BroadcastMuteState {
        muted: next.muted,
        deafened: next.deafened,
    });

    ctx.status_item.borrow_mut().set_mute_checked(next.muted);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> MuteState {
        MuteState {
            muted: false,
            deafened: false,
            muted_before_deafen: false,
        }
    }

    #[test]
    fn mute_toggles_and_reports_the_change() {
        let out = fresh().apply(MuteAction::ToggleMute);
        assert!(out.state.muted);
        assert_eq!(out.send_mute, Some(true));
        assert_eq!(out.send_deafen, None);

        let out = out.state.apply(MuteAction::ToggleMute);
        assert!(!out.state.muted);
        assert_eq!(out.send_mute, Some(false));
    }

    #[test]
    fn deafening_also_mutes() {
        let out = fresh().apply(MuteAction::ToggleDeafen);
        assert!(out.state.deafened);
        assert!(out.state.muted, "deafening must mute the mic");
        assert_eq!(out.send_deafen, Some(true));
        assert_eq!(out.send_mute, Some(true));
    }

    #[test]
    fn undeafening_restores_an_unmuted_mic() {
        let deafened = fresh().apply(MuteAction::ToggleDeafen).state;
        let out = deafened.apply(MuteAction::ToggleDeafen);
        assert!(!out.state.deafened);
        assert!(!out.state.muted, "the mic was open before deafening");
        assert_eq!(out.send_mute, Some(false));
    }

    /// The rule most easily broken by a rewrite: a deliberate mute must
    /// survive a deafen/undeafen round trip.
    #[test]
    fn undeafening_keeps_a_deliberate_mute() {
        let muted = fresh().apply(MuteAction::ToggleMute).state;
        let deafened = muted.apply(MuteAction::ToggleDeafen);
        assert_eq!(
            deafened.send_mute, None,
            "already muted, so no redundant SetMute"
        );

        let out = deafened.state.apply(MuteAction::ToggleDeafen);
        assert!(
            out.state.muted,
            "the user muted on purpose; undeafening must not unmute them"
        );
        assert_eq!(out.send_mute, None, "nothing changed, so nothing to send");
    }

    /// Unmuting clears deafen, so the user can always hear the call they are
    /// speaking into.
    #[test]
    fn unmuting_also_undeafens() {
        let deafened = fresh().apply(MuteAction::ToggleDeafen).state;
        assert!(deafened.deafened && deafened.muted);

        let out = deafened.apply(MuteAction::ToggleMute);
        assert!(!out.state.muted, "the mic opens");
        assert!(
            !out.state.deafened,
            "and deafen clears, rather than leaving the user talking into a \
             call they cannot hear"
        );
        assert_eq!(out.send_mute, Some(false));
        assert_eq!(
            out.send_deafen,
            Some(false),
            "the crew must be told deafen ended, not just mute"
        );
    }

    /// **The invariant.** No sequence of actions may leave the mic live while
    /// deafened. Exhaustive to four steps.
    #[test]
    fn deafened_never_coexists_with_a_live_mic() {
        let actions = [MuteAction::ToggleMute, MuteAction::ToggleDeafen];
        for a in actions {
            for b in actions {
                for c in actions {
                    for d in actions {
                        let mut s = fresh();
                        for action in [a, b, c, d] {
                            s = s.apply(action).state;
                            assert!(
                                !s.deafened || s.muted,
                                "{a:?} -> {b:?} -> {c:?} -> {d:?} reached \
                                 deafened with a live mic"
                            );
                        }
                    }
                }
            }
        }
    }
}
