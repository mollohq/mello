//! Playback volume and local mute of each voice member, for this user only.
//!
//! The member volume popover (crew panel) sets these values. They are kept
//! in memory for the life of the app, keyed by user id, and are not written
//! to the settings file: a restart sets every member back to 100 % and not
//! muted (spec 01 section 4.1).
//!
//! libmello names the receive state of a member after the user id, for SFU
//! and for P2P, so the user id is also the `peer_id` of
//! `Command::SetPeerVolume`.

use std::collections::{HashMap, HashSet};

use mello_core::Command;

/// The volume of a member with no entry.
pub const DEFAULT_PERCENT: u16 = 100;
/// The highest volume. libmello clamps the gain to 2.0 (`kMaxPeerVolume`).
pub const MAX_PERCENT: u16 = 200;

/// The playback settings of one member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberAudio {
    /// 0 to [`MAX_PERCENT`]. A local mute does not change it.
    pub volume_percent: u16,
    /// Muted for this user only. The member's own mic state is different.
    pub muted_locally: bool,
}

impl Default for MemberAudio {
    fn default() -> Self {
        Self {
            volume_percent: DEFAULT_PERCENT,
            muted_locally: false,
        }
    }
}

impl MemberAudio {
    /// The gain for libmello: 0.0 when muted, else the percent over 100.
    pub fn gain(self) -> f32 {
        if self.muted_locally {
            0.0
        } else {
            f32::from(self.volume_percent.min(MAX_PERCENT)) / 100.0
        }
    }

    fn is_default(self) -> bool {
        self == Self::default()
    }
}

/// The settings of every member this user changed, and the members of the
/// joined channel at the last sync.
#[derive(Debug, Default)]
pub struct MemberAudioTable {
    /// Only members that are not at the default have an entry.
    entries: HashMap<String, MemberAudio>,
    /// The members of the joined channel at the last [`Self::sync_present`].
    present: HashSet<String>,
}

impl MemberAudioTable {
    /// The settings of `user_id`. The default when it has no entry.
    pub fn get(&self, user_id: &str) -> MemberAudio {
        self.entries.get(user_id).copied().unwrap_or_default()
    }

    /// Set the volume of `user_id`, clamped to 0..=[`MAX_PERCENT`]. Returns
    /// the command that applies it. While the member is muted the command
    /// keeps the gain at 0.
    pub fn set_volume(&mut self, user_id: &str, percent: i32) -> Command {
        let percent = percent.clamp(0, i32::from(MAX_PERCENT)) as u16;
        self.update(user_id, |a| a.volume_percent = percent)
    }

    /// Mute or unmute `user_id` for this user. The volume stays. Returns the
    /// command that applies it.
    pub fn set_muted_locally(&mut self, user_id: &str, muted: bool) -> Command {
        self.update(user_id, |a| a.muted_locally = muted)
    }

    /// Record the members of the joined channel (`ids`; empty when not in a
    /// channel). Returns a command for each member that was not present at
    /// the last sync and has an entry: a join, a rejoin and a reconnect all
    /// send the stored value again.
    pub fn sync_present<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>) -> Vec<Command> {
        let now: HashSet<String> = ids.into_iter().map(str::to_string).collect();
        let mut out = Vec::new();
        for id in &now {
            if self.present.contains(id) {
                continue;
            }
            if let Some(audio) = self.entries.get(id) {
                log::info!("Member audio: {} is in the channel again, apply it", id);
                out.push(apply(id, *audio));
            }
        }
        self.present = now;
        out
    }

    fn update(&mut self, user_id: &str, f: impl FnOnce(&mut MemberAudio)) -> Command {
        let mut audio = self.get(user_id);
        f(&mut audio);
        if audio.is_default() {
            self.entries.remove(user_id);
        } else {
            self.entries.insert(user_id.to_string(), audio);
        }
        log::debug!(
            "Member audio: {} volume={}% muted_locally={}",
            user_id,
            audio.volume_percent,
            audio.muted_locally
        );
        apply(user_id, audio)
    }
}

/// The command that gives `user_id` the gain of `audio`.
fn apply(user_id: &str, audio: MemberAudio) -> Command {
    Command::SetPeerVolume {
        peer_id: user_id.to_string(),
        volume: audio.gain(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain_of(cmd: &Command) -> (String, f32) {
        match cmd {
            Command::SetPeerVolume { peer_id, volume } => (peer_id.clone(), *volume),
            other => panic!("expected SetPeerVolume, got {other:?}"),
        }
    }

    #[test]
    fn the_percent_maps_to_the_gain() {
        let at = |volume_percent| MemberAudio {
            volume_percent,
            muted_locally: false,
        };
        assert_eq!(at(0).gain(), 0.0);
        assert_eq!(at(70).gain(), 0.7);
        assert_eq!(at(100).gain(), 1.0);
        assert_eq!(at(200).gain(), 2.0);
        assert_eq!(MemberAudio::default().gain(), 1.0);
    }

    #[test]
    fn a_local_mute_sends_zero_and_keeps_the_volume() {
        let mut t = MemberAudioTable::default();
        assert_eq!(gain_of(&t.set_volume("u-bob", 70)), ("u-bob".into(), 0.7));
        assert_eq!(gain_of(&t.set_muted_locally("u-bob", true)).1, 0.0);
        assert_eq!(
            gain_of(&t.set_volume("u-bob", 150)).1,
            0.0,
            "a volume change while muted stays silent"
        );
        assert_eq!(t.get("u-bob").volume_percent, 150);
        assert_eq!(gain_of(&t.set_muted_locally("u-bob", false)).1, 1.5);
    }

    #[test]
    fn the_volume_is_capped_at_200_and_floored_at_0() {
        let mut t = MemberAudioTable::default();
        assert_eq!(gain_of(&t.set_volume("u-bob", 350)).1, 2.0);
        assert_eq!(t.get("u-bob").volume_percent, MAX_PERCENT);
        assert_eq!(gain_of(&t.set_volume("u-bob", -20)).1, 0.0);
        assert_eq!(t.get("u-bob").volume_percent, 0);
    }

    #[test]
    fn a_member_back_at_the_default_has_no_entry() {
        let mut t = MemberAudioTable::default();
        t.set_volume("u-bob", 70);
        t.set_volume("u-bob", 100);
        assert!(t.entries.is_empty());
        t.set_muted_locally("u-bob", true);
        t.set_muted_locally("u-bob", false);
        assert!(t.entries.is_empty());
    }

    #[test]
    fn the_stored_value_is_sent_again_when_the_member_appears_again() {
        let mut t = MemberAudioTable::default();
        t.set_volume("u-bob", 70);
        t.set_muted_locally("u-eve", true);

        let first: Vec<_> = t
            .sync_present(["u-me", "u-bob", "u-ann"])
            .iter()
            .map(gain_of)
            .collect();
        assert_eq!(
            first,
            vec![("u-bob".to_string(), 0.7)],
            "only bob has an entry"
        );

        assert!(
            t.sync_present(["u-me", "u-bob", "u-ann"]).is_empty(),
            "a member who stays is not sent again"
        );

        // bob leaves, eve joins; then bob comes back.
        let eve: Vec<_> = t
            .sync_present(["u-me", "u-eve"])
            .iter()
            .map(gain_of)
            .collect();
        assert_eq!(eve, vec![("u-eve".to_string(), 0.0)]);
        let bob: Vec<_> = t
            .sync_present(["u-me", "u-eve", "u-bob"])
            .iter()
            .map(gain_of)
            .collect();
        assert_eq!(bob, vec![("u-bob".to_string(), 0.7)]);
    }

    #[test]
    fn the_table_survives_leaving_the_channel() {
        let mut t = MemberAudioTable::default();
        t.set_volume("u-bob", 70);
        t.sync_present(["u-me", "u-bob"]);

        assert!(t.sync_present(std::iter::empty()).is_empty(), "leave");
        assert_eq!(t.get("u-bob").volume_percent, 70, "the leave keeps it");

        let rejoin: Vec<_> = t
            .sync_present(["u-me", "u-bob"])
            .iter()
            .map(gain_of)
            .collect();
        assert_eq!(
            rejoin,
            vec![("u-bob".to_string(), 0.7)],
            "the rejoin sends it"
        );
    }
}
