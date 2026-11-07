//! Notification sounds (plans/NOTIFICATION-SOUNDS.md).
//!
//! The sounds are 48 kHz stereo 16-bit WAV files, built into the binary.
//! [`SoundPlayer`] loads them into libmello once the voice context exists,
//! and plays them from any thread without the command loop, so a sound is
//! never late behind a slow RPC.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// One notification sound. Each has its own libmello slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sound {
    YouJoin,
    YouLeave,
    MemberJoin,
    MemberLeave,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    ConnectionLost,
    CrewLive,
    YouLive,
    StreamEnded,
    Message,
    Mention,
    ClipSaved,
    CrewClip,
    NewMember,
}

/// The settings group that turns a sound on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoundGroup {
    Voice,
    Streams,
    Chat,
    Clips,
    Crew,
}

impl Sound {
    pub const ALL: [Sound; 17] = [
        Sound::YouJoin,
        Sound::YouLeave,
        Sound::MemberJoin,
        Sound::MemberLeave,
        Sound::Mute,
        Sound::Unmute,
        Sound::Deafen,
        Sound::Undeafen,
        Sound::ConnectionLost,
        Sound::CrewLive,
        Sound::YouLive,
        Sound::StreamEnded,
        Sound::Message,
        Sound::Mention,
        Sound::ClipSaved,
        Sound::CrewClip,
        Sound::NewMember,
    ];

    /// The libmello slot (`mello_sound_load`, `mello_sound_play`).
    pub fn slot(self) -> u32 {
        self as u32
    }

    pub fn group(self) -> SoundGroup {
        match self {
            Sound::YouJoin
            | Sound::YouLeave
            | Sound::MemberJoin
            | Sound::MemberLeave
            | Sound::Mute
            | Sound::Unmute
            | Sound::Deafen
            | Sound::Undeafen
            | Sound::ConnectionLost => SoundGroup::Voice,
            Sound::CrewLive | Sound::YouLive | Sound::StreamEnded => SoundGroup::Streams,
            Sound::Message | Sound::Mention => SoundGroup::Chat,
            Sound::ClipSaved | Sound::CrewClip => SoundGroup::Clips,
            Sound::NewMember => SoundGroup::Crew,
        }
    }

    fn wav(self) -> &'static [u8] {
        macro_rules! wav {
            ($name:literal) => {
                include_bytes!(concat!("../assets/sounds/", $name, ".wav"))
            };
        }
        match self {
            Sound::YouJoin => wav!("you_join"),
            Sound::YouLeave => wav!("you_leave"),
            Sound::MemberJoin => wav!("member_join"),
            Sound::MemberLeave => wav!("member_leave"),
            Sound::Mute => wav!("mute"),
            Sound::Unmute => wav!("unmute"),
            Sound::Deafen => wav!("deafen"),
            Sound::Undeafen => wav!("undeafen"),
            Sound::ConnectionLost => wav!("connection_lost"),
            Sound::CrewLive => wav!("crew_live"),
            Sound::YouLive => wav!("you_live"),
            Sound::StreamEnded => wav!("stream_ended"),
            Sound::Message => wav!("message"),
            Sound::Mention => wav!("mention"),
            Sound::ClipSaved => wav!("clip_saved"),
            Sound::CrewClip => wav!("crew_clip"),
            Sound::NewMember => wav!("new_member"),
        }
    }

    /// The sound as interleaved 48 kHz stereo PCM.
    pub fn pcm(self) -> Result<Vec<i16>, WavError> {
        decode_wav(self.wav())
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum WavError {
    #[error("not a RIFF WAVE file")]
    NotWave,
    #[error("no fmt or data chunk")]
    MissingChunk,
    #[error(
        "format {format}, {channels} ch, {rate} Hz, {bits} bit: need PCM, 2 ch, 48000 Hz, 16 bit"
    )]
    Unsupported {
        format: u16,
        channels: u16,
        rate: u32,
        bits: u16,
    },
}

/// Decodes a 48 kHz stereo 16-bit PCM WAV file into interleaved samples.
/// It walks the chunks, so extra chunks (LIST and others) are skipped.
pub fn decode_wav(bytes: &[u8]) -> Result<Vec<i16>, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32_at =
        |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);

    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(pos + 4) as usize;
        let body = pos + 8;
        let end = body.saturating_add(len).min(bytes.len());
        if id == b"fmt " && end - body >= 16 {
            fmt = Some((
                u16_at(body),
                u16_at(body + 2),
                u32_at(body + 4),
                u16_at(body + 14),
            ));
        } else if id == b"data" {
            data = Some(&bytes[body..end]);
        }
        pos = body.saturating_add(len).saturating_add(len & 1);
    }

    let (Some((format, channels, rate, bits)), Some(data)) = (fmt, data) else {
        return Err(WavError::MissingChunk);
    };
    if format != 1 || channels != 2 || rate != 48_000 || bits != 16 {
        return Err(WavError::Unsupported {
            format,
            channels,
            rate,
            bits,
        });
    }
    // Whole stereo frames only.
    let usable = data.len() / 4 * 4;
    Ok(data[..usable]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect())
}

/// The libmello context that sounds play on. The pointer is only used under
/// the player's mutex, and the voice manager clears it before it destroys
/// the context.
struct Ctx(*mut mello_sys::MelloContext);

// SAFETY: `mello_sound_play` is safe from any thread (it takes the mixer's
// own lock), and the pointer is read and cleared only under the mutex.
unsafe impl Send for Ctx {}

/// Plays notification sounds. Cheap to clone; any thread.
///
/// The client makes one at startup and gives a clone to the core client,
/// which attaches the voice context and loads every sound. A play before
/// that, or after shutdown, does nothing.
#[derive(Clone, Default)]
pub struct SoundPlayer {
    ctx: Arc<Mutex<Option<Ctx>>>,
}

impl SoundPlayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Plays `sound` once at `volume` (0..1). Returns false when no context
    /// is attached or libmello refused it.
    pub fn play(&self, sound: Sound, volume: f32) -> bool {
        let guard = self.ctx.lock().unwrap_or_else(|e| e.into_inner());
        let Some(Ctx(ctx)) = guard.as_ref() else {
            log::debug!("sound {sound:?}: no voice context yet");
            return false;
        };
        let rc = unsafe { mello_sys::mello_sound_play(*ctx, sound.slot(), volume.clamp(0.0, 1.0)) };
        if rc != mello_sys::MelloResult_MELLO_OK {
            log::warn!("sound {sound:?}: mello_sound_play failed ({rc})");
            return false;
        }
        log::debug!("sound {sound:?} at {volume:.2}");
        true
    }

    /// Loads every sound into `ctx` and plays on it from now on.
    pub(crate) fn attach(&self, ctx: *mut mello_sys::MelloContext) {
        if ctx.is_null() {
            return;
        }
        let mut loaded = 0;
        for sound in Sound::ALL {
            match sound.pcm() {
                Ok(pcm) => {
                    let frames = pcm.len() / 2;
                    let rc = unsafe {
                        mello_sys::mello_sound_load(ctx, sound.slot(), pcm.as_ptr(), frames)
                    };
                    if rc == mello_sys::MelloResult_MELLO_OK {
                        loaded += 1;
                    } else {
                        log::error!("sound {sound:?}: mello_sound_load failed ({rc})");
                    }
                }
                Err(e) => log::error!("sound {sound:?}: bad asset: {e}"),
            }
        }
        log::info!(
            "notification sounds: {loaded} of {} loaded",
            Sound::ALL.len()
        );
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ctx(ctx));
    }

    /// Stops using the context. Call before the context is destroyed.
    pub(crate) fn detach(&self) {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_decodes_as_48k_stereo() {
        for sound in Sound::ALL {
            let pcm = sound.pcm().unwrap_or_else(|e| panic!("{sound:?}: {e}"));
            let seconds = pcm.len() as f32 / 2.0 / 48_000.0;
            assert!(
                (0.3..=3.0).contains(&seconds),
                "{sound:?} is {seconds:.2} s"
            );
            assert!(pcm.iter().any(|&s| s != 0), "{sound:?} is silent");
        }
    }

    #[test]
    fn slots_are_unique_and_fit_libmello() {
        let mut slots: Vec<u32> = Sound::ALL.iter().map(|s| s.slot()).collect();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), Sound::ALL.len());
        assert!(slots.iter().all(|&s| s < mello_sys::MELLO_SOUND_SLOTS));
    }

    fn wav(format: u16, channels: u16, rate: u32, bits: u16, extra: &[u8], pcm: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&format.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        b.extend_from_slice(&(channels * 2).to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(extra);
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        b.extend_from_slice(pcm);
        b
    }

    #[test]
    fn decode_skips_other_chunks_and_partial_frames() {
        // A 3-byte LIST chunk with its pad byte, then 1.5 frames of data.
        let extra = [b"LIST".as_slice(), &3u32.to_le_bytes(), &[1, 2, 3, 0]].concat();
        let pcm = [1, 0, 2, 0, 3, 0];
        let b = wav(1, 2, 48_000, 16, &extra, &pcm);
        assert_eq!(decode_wav(&b), Ok(vec![1, 2]));
    }

    #[test]
    fn decode_rejects_other_formats() {
        assert_eq!(decode_wav(b"nope"), Err(WavError::NotWave));
        assert!(matches!(
            decode_wav(&wav(1, 1, 48_000, 16, &[], &[0, 0])),
            Err(WavError::Unsupported { channels: 1, .. })
        ));
        assert!(matches!(
            decode_wav(&wav(1, 2, 44_100, 16, &[], &[0, 0, 0, 0])),
            Err(WavError::Unsupported { rate: 44_100, .. })
        ));
    }

    #[test]
    fn a_player_without_a_context_does_nothing() {
        assert!(!SoundPlayer::new().play(Sound::Mute, 1.0));
    }

    #[test]
    fn a_player_plays_on_the_voice_context_until_it_is_destroyed() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut voice = crate::voice::VoiceManager::with_test_audio_backend(tx);
        let player = SoundPlayer::new();
        voice.attach_sound_player(player.clone());
        // The test backend has no render thread, so plays wait in libmello's
        // queue (16 deep). A few are enough.
        for sound in [Sound::YouJoin, Sound::Mention, Sound::NewMember] {
            assert!(player.play(sound, 0.8), "{sound:?}");
        }
        drop(voice);
        assert!(
            !player.play(Sound::Mute, 0.8),
            "a play after the context is gone"
        );
    }
}
