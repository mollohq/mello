use crate::voice::VoiceMode;

use super::Client;

impl Client {
    /// Whether the 20ms voice tick should run (audio poll, SFU liveness, reconnect).
    pub(super) fn needs_voice_tick(&self) -> bool {
        if self.voice.needs_periodic_tick() {
            return true;
        }
        if self.clip_was_playing {
            return true;
        }
        if self.sfu_voice_reconnect.is_some() {
            return true;
        }
        // Reconnect scheduler after an SFU drop while we still remember the channel.
        if self.last_voice_channel.is_some() && self.voice.voice_mode() == VoiceMode::Disconnected {
            return true;
        }
        false
    }

    /// Whether the 16ms stream tick should run (signal drain, viewer decode, host pacing).
    pub(super) fn needs_stream_tick(&self) -> bool {
        self.stream_session.is_some() || self.viewer_state.is_some()
    }
}
