//! Whether the user is at the app right now.
//!
//! One definition, shared by remote push (spec 23 §6.2: an active desktop
//! holds phone pushes back) and the updater's idle handoff
//! (`mello-backlog/plans/update-system.md`, T1: only apply while inactive).

/// Input idle time after which a focused window no longer counts as "at the app".
pub const INPUT_IDLE_LIMIT_SECS: u64 = 600;

/// The signals the activity decision reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivityInputs {
    /// The app window is visible and focused (iOS: the app is in the foreground).
    pub foreground: bool,
    /// Seconds since the last keyboard or mouse input anywhere on the system.
    pub input_idle_secs: u64,
    pub in_voice: bool,
    pub hosting_stream: bool,
    pub game_running: bool,
}

/// Active when the app is in front and used, or the user is in voice, hosts a
/// stream, or plays a game. In all of those the user is at the device.
pub fn is_active(i: &ActivityInputs) -> bool {
    (i.foreground && i.input_idle_secs < INPUT_IDLE_LIMIT_SECS)
        || i.in_voice
        || i.hosting_stream
        || i.game_running
}

/// The platform name the push service expects for this build.
pub fn platform() -> &'static str {
    if cfg!(target_os = "ios") {
        "ios"
    } else {
        "desktop"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(foreground: bool, idle: u64) -> ActivityInputs {
        ActivityInputs {
            foreground,
            input_idle_secs: idle,
            ..Default::default()
        }
    }

    #[test]
    fn a_focused_window_in_use_is_active() {
        assert!(is_active(&inputs(true, 0)));
        assert!(is_active(&inputs(true, INPUT_IDLE_LIMIT_SECS - 1)));
    }

    #[test]
    fn a_hidden_window_or_an_idle_user_is_inactive() {
        assert!(!is_active(&inputs(false, 0)));
        assert!(!is_active(&inputs(true, INPUT_IDLE_LIMIT_SECS)));
    }

    #[test]
    fn voice_streaming_or_a_game_keeps_the_user_active_behind_other_windows() {
        let away = inputs(false, 3600);
        for i in [
            ActivityInputs {
                in_voice: true,
                ..away
            },
            ActivityInputs {
                hosting_stream: true,
                ..away
            },
            ActivityInputs {
                game_running: true,
                ..away
            },
        ] {
            assert!(is_active(&i), "{i:?}");
        }
    }
}
