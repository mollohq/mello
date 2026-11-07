//! Notification sounds: which event plays which sound, and the guards that
//! keep a snapshot, a reconnect or a burst from playing false or repeated
//! sounds (plans/NOTIFICATION-SOUNDS.md §5).
//!
//! The handlers call [`cue`] and the helpers below. [`SoundCues`] checks the
//! settings group and the rate limit, and plays through the core's
//! [`SoundPlayer`], which does not go through the command loop.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use mello_core::crew_state::CrewEvent;
use mello_core::sounds::{Sound, SoundGroup, SoundPlayer};

use crate::app_context::AppContext;
use crate::Settings;

/// Recent cues kept for tests and the e2e state port.
const LOG_LEN: usize = 64;

/// The other members of the voice channel the user is in, for the join and
/// leave sounds.
struct Roster {
    channel: String,
    members: BTreeSet<String>,
}

pub struct SoundCues {
    /// `None` in tests: the cue is logged, nothing plays.
    player: Option<SoundPlayer>,
    roster: RefCell<Option<Roster>>,
    last_played: RefCell<HashMap<Sound, Instant>>,
    log: RefCell<Vec<Sound>>,
}

impl SoundCues {
    pub fn new(player: Option<SoundPlayer>) -> Self {
        Self {
            player,
            roster: RefCell::new(None),
            last_played: RefCell::new(HashMap::new()),
            log: RefCell::new(Vec::new()),
        }
    }

    /// Plays `sound` when its group is on and its rate limit allows it.
    /// Returns true when it played.
    pub fn cue(&self, settings: &Settings, sound: Sound) -> bool {
        if !settings.sound_group_enabled(sound.group()) {
            log::debug!("sound {sound:?}: group off");
            return false;
        }
        let now = Instant::now();
        if let Some(gap) = min_gap(sound) {
            let mut last = self.last_played.borrow_mut();
            if last
                .get(&sound)
                .is_some_and(|t| now.duration_since(*t) < gap)
            {
                log::debug!("sound {sound:?}: rate limited");
                return false;
            }
            last.insert(sound, now);
        }
        {
            let mut log = self.log.borrow_mut();
            if log.len() == LOG_LEN {
                log.remove(0);
            }
            log.push(sound);
        }
        log::info!("sound: {sound:?}");
        if let Some(player) = &self.player {
            player.play(sound, settings.sound_volume);
        }
        true
    }

    /// The sounds cued, oldest first (at most the last 64).
    #[cfg(test)]
    pub fn recent(&self) -> Vec<Sound> {
        self.log.borrow().clone()
    }

    /// Diffs the other members of the user's voice channel against the last
    /// call. Returns the sound to play: a join wins over a leave.
    ///
    /// The first list after the user joins a channel seeds the roster and
    /// plays nothing: those members were there before the user.
    fn sync_voice_members(
        &self,
        in_call: bool,
        channel: &str,
        members: BTreeSet<String>,
    ) -> Option<Sound> {
        let mut roster = self.roster.borrow_mut();
        if !in_call || channel.is_empty() {
            *roster = None;
            return None;
        }
        let Some(prev) = roster.as_mut().filter(|r| r.channel == channel) else {
            *roster = Some(Roster {
                channel: channel.to_string(),
                members,
            });
            return None;
        };
        let joined = members.difference(&prev.members).next().is_some();
        let left = prev.members.difference(&members).next().is_some();
        prev.members = members;
        if joined {
            Some(Sound::MemberJoin)
        } else if left {
            Some(Sound::MemberLeave)
        } else {
            None
        }
    }
}

/// The shortest time between two plays of one sound. A burst (three members
/// join at once, a fast chat) plays once.
fn min_gap(sound: Sound) -> Option<Duration> {
    match sound {
        Sound::MemberJoin | Sound::MemberLeave => Some(Duration::from_millis(500)),
        Sound::Message | Sound::Mention => Some(Duration::from_millis(1000)),
        _ => None,
    }
}

pub(crate) fn cue(ctx: &AppContext, sound: Sound) {
    ctx.sounds.cue(&ctx.settings.borrow(), sound);
}

/// The sound for a change of the local mute and deafen state. A deafen that
/// also mutes plays the deafen sound only.
pub(crate) fn cue_mute_change(ctx: &AppContext, before: (bool, bool), after: (bool, bool)) {
    let (was_muted, was_deafened) = before;
    let (muted, deafened) = after;
    let sound = if deafened != was_deafened {
        if deafened {
            Sound::Deafen
        } else {
            Sound::Undeafen
        }
    } else if muted != was_muted {
        if muted {
            Sound::Mute
        } else {
            Sound::Unmute
        }
    } else {
        return;
    };
    cue(ctx, sound);
}

/// Called with the other members of the joined channel after each change of
/// the voice models (`handlers::voice::sync_member_audio`).
pub(crate) fn sync_voice_members(ctx: &AppContext, channel: &str, members: &[String]) {
    let set = members.iter().cloned().collect();
    if let Some(sound) = ctx
        .sounds
        .sync_voice_members(ctx.in_sfu_call.get(), channel, set)
    {
        cue(ctx, sound);
    }
}

/// The sound for a crew event (code 111) of the active crew that someone
/// else caused.
pub(crate) fn cue_crew_event(ctx: &AppContext, event: &CrewEvent) {
    if event.crew_id != ctx.app.get_active_crew_id().as_str() {
        return;
    }
    let (sound, actor_key) = match event.event.as_str() {
        "stream_started" => (Sound::CrewLive, "streamer_id"),
        "clip_posted" => (Sound::CrewClip, "clipper_id"),
        "member_joined" => (Sound::NewMember, "user_id"),
        _ => return,
    };
    let actor = event.data.get(actor_key).and_then(|v| v.as_str());
    if actor.is_none_or(|a| a == ctx.app.get_user_id().as_str()) {
        return;
    }
    cue(ctx, sound);
}

impl Settings {
    /// The sound settings as the Audio tab shows them.
    pub fn sound_settings(&self) -> crate::SoundSettings {
        crate::SoundSettings {
            volume: self.sound_volume,
            voice: self.sound_voice,
            streams: self.sound_streams,
            chat: self.sound_chat,
            clips: self.sound_clips,
            crew: self.sound_crew,
        }
    }

    /// Takes the sound settings from the Audio tab.
    pub fn set_sound_settings(&mut self, v: &crate::SoundSettings) {
        self.sound_volume = v.volume.clamp(0.0, 1.0);
        self.sound_voice = v.voice;
        self.sound_streams = v.streams;
        self.sound_chat = v.chat;
        self.sound_clips = v.clips;
        self.sound_crew = v.crew;
    }

    /// Whether the settings group of a sound is on.
    pub fn sound_group_enabled(&self, group: SoundGroup) -> bool {
        match group {
            SoundGroup::Voice => self.sound_voice,
            SoundGroup::Streams => self.sound_streams,
            SoundGroup::Chat => self.sound_chat,
            SoundGroup::Clips => self.sound_clips,
            SoundGroup::Crew => self.sound_crew,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_first_member_list_seeds_and_plays_nothing() {
        let cues = SoundCues::new(None);
        assert_eq!(cues.sync_voice_members(true, "ch", set(&["a", "b"])), None);
        assert_eq!(
            cues.sync_voice_members(true, "ch", set(&["a", "b", "c"])),
            Some(Sound::MemberJoin)
        );
        assert_eq!(
            cues.sync_voice_members(true, "ch", set(&["a", "c"])),
            Some(Sound::MemberLeave)
        );
        assert_eq!(cues.sync_voice_members(true, "ch", set(&["a", "c"])), None);
    }

    #[test]
    fn leaving_the_call_or_moving_channel_seeds_again() {
        let cues = SoundCues::new(None);
        cues.sync_voice_members(true, "ch", set(&["a"]));
        assert_eq!(cues.sync_voice_members(false, "", set(&[])), None);
        assert_eq!(cues.sync_voice_members(true, "ch", set(&["a", "b"])), None);
        assert_eq!(cues.sync_voice_members(true, "other", set(&["x"])), None);
        assert_eq!(
            cues.sync_voice_members(true, "other", set(&["x", "y"])),
            Some(Sound::MemberJoin)
        );
    }

    #[test]
    fn a_join_and_a_leave_together_play_the_join() {
        let cues = SoundCues::new(None);
        cues.sync_voice_members(true, "ch", set(&["a"]));
        assert_eq!(
            cues.sync_voice_members(true, "ch", set(&["b"])),
            Some(Sound::MemberJoin)
        );
    }

    #[test]
    fn a_group_that_is_off_plays_nothing() {
        let cues = SoundCues::new(None);
        let settings = Settings {
            sound_chat: false,
            ..Settings::default()
        };
        assert!(!cues.cue(&settings, Sound::Message));
        assert!(cues.cue(&settings, Sound::Mute));
        assert_eq!(cues.recent(), vec![Sound::Mute]);
    }

    #[test]
    fn a_burst_plays_once() {
        let cues = SoundCues::new(None);
        let settings = Settings::default();
        assert!(cues.cue(&settings, Sound::MemberJoin));
        assert!(!cues.cue(&settings, Sound::MemberJoin));
        // Another sound has its own limit.
        assert!(cues.cue(&settings, Sound::MemberLeave));
        // Sounds without a limit always play.
        assert!(cues.cue(&settings, Sound::Mute));
        assert!(cues.cue(&settings, Sound::Mute));
    }
}
