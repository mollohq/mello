//! The stream link of a host or a viewer: when the UI shows "reconnecting",
//! when the client rejoins the SFU, when it gives up (streaming-quality
//! stage 6).
//!
//! Pure decision logic with injected clocks, like `reconnect.rs` for the
//! Nakama socket. The command loop gives it what it sees on each stream
//! tick and acts on what it returns.
//!
//! | Signal | Meaning | Action |
//! |---|---|---|
//! | No control-channel pong for 700 ms | Own link silent (a pulled cable) | Show "reconnecting"; the connection stays |
//! | Pong back within 5 s | Own link back | Resume (host at once, viewer at its next frame) |
//! | Silent for 5 s, socket closed, ICE dead, `PEER_SLOW` | The connection is gone | Rejoin on the voice schedule: 0.5 s, 1 s, then every 2 s |
//! | Viewer: no host media for 700 ms, own link fine | The host is gone | Show "reconnecting"; the SFU holds the session |
//! | 30 s after the drop with no rejoin, or no frame | | Give up: the stream ends with a message |
//!
//! The SFU marks a silent peer disconnected after 3 s and failed after 8 s
//! (`SetICETimeouts(3 s, 5 s, 2 s)`, mello-sfu stage 5). Its ICE restart
//! offer on `Failed` cannot work here: libjuice 1.7 refuses a second remote
//! description ("ICE restart is not supported"). A link silent for 5 s
//! therefore rejoins with a new connection in the same session; the SFU
//! replaces the old peer of the same user (host: `peer_evicted`, or
//! `host_reconnected` after the hold).

use std::time::{Duration, Instant};

use crate::events::StreamRole;

use super::voice::{sfu_reconnect_delay, SFU_RECONNECT_GIVE_UP_AFTER};

/// No pong for this long: the link is silent, the UI shows "reconnecting".
/// The same value as the voice link (spec 10 section 7.6).
pub(super) const LINK_STALL: Duration = Duration::from_millis(700);
/// A silence this long ends the connection: the SFU has marked the peer
/// disconnected (3 s) and fails it at 8 s.
pub(super) const LINK_REBUILD: Duration = Duration::from_secs(5);
/// Viewer: no host media for this long while the host is not paused or
/// idle (the idle keepalive sends every 500 ms).
pub(super) const HOST_SILENT: Duration = Duration::from_millis(700);
/// The control-channel ping cadence of a stream link: every 125 stream
/// ticks (2 s), as before stage 6, for the RTT. mello-sfu forwards a
/// viewer's control messages to the host and answers control-channel pings
/// for voice only, so a faster ping would load the host with every viewer's
/// pings and give no pong. The signaling socket's ping (every 128 ms,
/// `SfuConnection`) carries the liveness.
pub(super) const LINK_PING_EVERY_TICKS: u64 = 125;

/// How long the own link of `conn` has been silent: the younger of the
/// signaling socket's pong age and the control channel's pong age. Each
/// counts from `connected_at` before its first pong. mello-sfu answers
/// control-channel pings for voice only, so for a stream the socket's pong
/// carries the liveness today; a pulled cable silences both.
pub(super) fn own_link_silence(
    conn: &crate::transport::SfuConnection,
    connected_at: Instant,
    now: Instant,
) -> Duration {
    let since_connect = now.saturating_duration_since(connected_at);
    let ws = Duration::from_millis(conn.ws_silent_ms()).min(since_connect);
    let dc = match conn.pong_age_ms() {
        age if age < 0 => since_connect,
        age => Duration::from_millis(age as u64),
    };
    ws.min(dc)
}

/// What the command loop does after an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkAction {
    /// `Event::StreamReconnecting`.
    ShowReconnecting,
    /// `Event::StreamResumed`.
    ShowResumed,
    /// Start a rejoin attempt now (one runs at a time).
    Rejoin,
    /// No rejoin within 30 s: end the stream with a message.
    GiveUp,
}

/// What the loop sees on one stream tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LinkObservation {
    /// How long the control channel of the current connection has been
    /// silent: the pong age, or the time since the connection opened when
    /// no pong came yet.
    pub own_silent: Duration,
    /// Viewer: how long no host media arrived. `None` for a host, while
    /// the host is paused or idle, and before the first frame.
    pub host_silent: Option<Duration>,
    /// Viewer: a new frame was shown since the last observation.
    pub new_frame: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    /// This side's own link.
    OwnLink,
    /// Viewer: the host's media stopped.
    Host,
}

#[derive(Debug, Clone, Copy)]
struct Rebuild {
    /// When the link was lost: the start of the silence, or the drop.
    dropped_at: Instant,
    /// The attempt that runs or comes next (0 is the first).
    attempt: u32,
    next_at: Instant,
    running: bool,
    /// When the attempt that runs started: the next one is due a schedule
    /// step after it, not after its end. An attempt that hangs (a connect
    /// on a pulled cable) then costs no extra gap when the cable is back.
    started_at: Instant,
}

/// The reconnect state of one stream link.
#[derive(Debug)]
pub(super) struct StreamLink {
    role: StreamRole,
    /// Since when the UI shows "reconnecting"; `None` while live.
    reconnecting_since: Option<Instant>,
    cause: Option<Cause>,
    rebuild: Option<Rebuild>,
    gave_up: bool,
}

impl StreamLink {
    pub fn new(role: StreamRole) -> Self {
        Self {
            role,
            reconnecting_since: None,
            cause: None,
            rebuild: None,
            gave_up: false,
        }
    }

    /// The UI shows "reconnecting".
    pub fn is_reconnecting(&self) -> bool {
        self.reconnecting_since.is_some()
    }

    /// The connection is gone and rejoin attempts run.
    pub fn is_rebuilding(&self) -> bool {
        self.rebuild.is_some()
    }

    /// A rejoin attempt runs now.
    #[cfg(test)]
    pub fn attempt_running(&self) -> bool {
        self.rebuild.is_some_and(|r| r.running)
    }

    /// The attempt that runs or comes next, from 1.
    pub fn attempt_number(&self) -> u32 {
        self.rebuild.map_or(0, |r| r.attempt + 1)
    }

    fn show_reconnecting(&mut self, since: Instant, cause: Cause, actions: &mut Vec<LinkAction>) {
        if self.reconnecting_since.is_none() {
            self.reconnecting_since = Some(since);
            actions.push(LinkAction::ShowReconnecting);
        }
        // The own link wins over the host as the cause: it decides rejoins.
        if self.cause != Some(Cause::OwnLink) {
            self.cause = Some(cause);
        }
    }

    fn resume(&mut self, actions: &mut Vec<LinkAction>) {
        if self.reconnecting_since.take().is_some() {
            actions.push(LinkAction::ShowResumed);
        }
        self.cause = None;
    }

    fn give_up(&mut self, actions: &mut Vec<LinkAction>) {
        if !self.gave_up {
            self.gave_up = true;
            actions.push(LinkAction::GiveUp);
        }
    }

    fn start_rebuild(&mut self, now: Instant, dropped_at: Instant) {
        if self.rebuild.is_none() {
            self.rebuild = Some(Rebuild {
                dropped_at,
                attempt: 0,
                // The first attempt 500 ms after the drop, as voice.
                next_at: (dropped_at + sfu_reconnect_delay(0)).max(now),
                running: false,
                started_at: now,
            });
        }
    }

    /// The connection is gone: the signaling socket closed, ICE failed,
    /// the SFU sent `PEER_SLOW`, or the machine woke from sleep.
    pub fn on_dropped(&mut self, now: Instant) -> Vec<LinkAction> {
        let mut actions = Vec::new();
        if self.gave_up {
            return actions;
        }
        let dropped_at = self.reconnecting_since.unwrap_or(now);
        self.show_reconnecting(now, Cause::OwnLink, &mut actions);
        self.cause = Some(Cause::OwnLink);
        self.start_rebuild(now, dropped_at);
        actions.extend(self.poll_rebuild(now));
        actions
    }

    /// Viewer: the SFU says the host disconnected and holds the session.
    pub fn on_host_reconnecting(&mut self, now: Instant) -> Vec<LinkAction> {
        let mut actions = Vec::new();
        if !self.gave_up {
            self.show_reconnecting(now, Cause::Host, &mut actions);
        }
        actions
    }

    /// The rejoin attempt failed: the next one follows on the schedule.
    pub fn on_attempt_failed(&mut self, now: Instant) {
        if let Some(r) = self.rebuild.as_mut() {
            r.running = false;
            r.attempt += 1;
            r.next_at = (r.started_at + sfu_reconnect_delay(r.attempt)).max(now);
        }
    }

    /// A rejoin attempt has a new connection in the same session. A host
    /// is live again at once. A viewer shows "reconnecting" until its next
    /// frame.
    pub fn on_rejoined(&mut self, now: Instant) -> Vec<LinkAction> {
        let mut actions = Vec::new();
        self.rebuild = None;
        match self.role {
            StreamRole::Host => self.resume(&mut actions),
            StreamRole::Viewer => {
                if self.reconnecting_since.is_none() {
                    self.reconnecting_since = Some(now);
                    actions.push(LinkAction::ShowReconnecting);
                }
                self.cause = Some(Cause::Host);
            }
        }
        actions
    }

    fn poll_rebuild(&mut self, now: Instant) -> Vec<LinkAction> {
        let mut actions = Vec::new();
        let Some(r) = self.rebuild.as_mut() else {
            return actions;
        };
        if now.duration_since(r.dropped_at) >= SFU_RECONNECT_GIVE_UP_AFTER {
            self.give_up(&mut actions);
            return actions;
        }
        if !r.running && now >= r.next_at {
            r.running = true;
            r.started_at = now;
            actions.push(LinkAction::Rejoin);
        }
        actions
    }

    /// One stream tick.
    pub fn observe(&mut self, now: Instant, seen: LinkObservation) -> Vec<LinkAction> {
        if self.gave_up {
            return Vec::new();
        }
        if self.rebuild.is_some() {
            return self.poll_rebuild(now);
        }
        let mut actions = Vec::new();

        // The own link first: it decides rejoins.
        if seen.own_silent >= LINK_STALL {
            let silent_since = now.checked_sub(seen.own_silent).unwrap_or(now);
            self.show_reconnecting(silent_since, Cause::OwnLink, &mut actions);
            self.cause = Some(Cause::OwnLink);
            if seen.own_silent >= LINK_REBUILD {
                let dropped_at = self.reconnecting_since.unwrap_or(silent_since);
                self.start_rebuild(now, dropped_at);
                actions.extend(self.poll_rebuild(now));
            }
            return actions;
        }

        // The own link answers.
        match self.role {
            StreamRole::Host => {
                if self.is_reconnecting() {
                    self.resume(&mut actions);
                }
            }
            StreamRole::Viewer => {
                if seen.new_frame {
                    self.resume(&mut actions);
                } else if seen.host_silent.is_some_and(|s| s >= HOST_SILENT) {
                    let since = now
                        .checked_sub(seen.host_silent.unwrap_or_default())
                        .unwrap_or(now);
                    self.show_reconnecting(since, Cause::Host, &mut actions);
                }
                if let Some(since) = self.reconnecting_since {
                    if now.duration_since(since) >= SFU_RECONNECT_GIVE_UP_AFTER {
                        self.give_up(&mut actions);
                    }
                }
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn host_seen(own_silent_ms: u64) -> LinkObservation {
        LinkObservation {
            own_silent: ms(own_silent_ms),
            host_silent: None,
            new_frame: false,
        }
    }

    fn viewer_seen(
        own_silent_ms: u64,
        host_silent_ms: Option<u64>,
        new_frame: bool,
    ) -> LinkObservation {
        LinkObservation {
            own_silent: ms(own_silent_ms),
            host_silent: host_silent_ms.map(ms),
            new_frame,
        }
    }

    #[test]
    fn a_short_silence_shows_reconnecting_and_resumes_on_the_same_connection() {
        let t0 = Instant::now();
        let mut link = StreamLink::new(StreamRole::Host);
        assert!(link.observe(t0, host_seen(100)).is_empty());
        assert_eq!(
            link.observe(t0 + ms(700), host_seen(700)),
            [LinkAction::ShowReconnecting]
        );
        assert!(link.observe(t0 + ms(2_000), host_seen(2_000)).is_empty());
        assert_eq!(
            link.observe(t0 + ms(3_000), host_seen(50)),
            [LinkAction::ShowResumed]
        );
        assert!(!link.is_rebuilding());
    }

    #[test]
    fn a_silence_of_5_s_rejoins_at_once() {
        let t0 = Instant::now();
        let mut link = StreamLink::new(StreamRole::Host);
        link.observe(t0 + ms(800), host_seen(800));
        assert_eq!(
            link.observe(t0 + ms(5_000), host_seen(5_000)),
            [LinkAction::Rejoin]
        );
        assert!(link.is_rebuilding() && link.attempt_running());
    }

    /// The voice schedule: 500 ms after the drop, then 1 s, then every 2 s
    /// after each failed attempt; the give-up 30 s after the drop.
    #[test]
    fn rejoins_follow_the_voice_schedule_and_give_up_30_s_after_the_drop() {
        let t0 = Instant::now();
        let mut link = StreamLink::new(StreamRole::Host);
        assert_eq!(link.on_dropped(t0), [LinkAction::ShowReconnecting]);
        assert!(link.observe(t0 + ms(499), host_seen(0)).is_empty());
        assert_eq!(
            link.observe(t0 + ms(500), host_seen(0)),
            [LinkAction::Rejoin]
        );
        assert!(
            link.observe(t0 + ms(600), host_seen(0)).is_empty(),
            "one attempt at a time"
        );

        // The next attempt is due a step after the start of the one before
        // (500 ms here), not after its end.
        link.on_attempt_failed(t0 + ms(600));
        assert!(link.observe(t0 + ms(1_499), host_seen(0)).is_empty());
        assert_eq!(
            link.observe(t0 + ms(1_500), host_seen(0)),
            [LinkAction::Rejoin]
        );
        // An attempt that hung past its step goes again at once.
        link.on_attempt_failed(t0 + ms(4_000));
        assert_eq!(
            link.observe(t0 + ms(4_000), host_seen(0)),
            [LinkAction::Rejoin]
        );
        link.on_attempt_failed(t0 + ms(4_100));
        assert!(link.observe(t0 + ms(5_999), host_seen(0)).is_empty());
        assert_eq!(
            link.observe(t0 + ms(6_000), host_seen(0)),
            [LinkAction::Rejoin]
        );

        assert_eq!(
            link.observe(t0 + ms(30_000), host_seen(0)),
            [LinkAction::GiveUp]
        );
        assert!(link.observe(t0 + ms(31_000), host_seen(0)).is_empty());
    }

    #[test]
    fn a_host_rejoin_resumes_at_once() {
        let t0 = Instant::now();
        let mut link = StreamLink::new(StreamRole::Host);
        link.on_dropped(t0);
        link.observe(t0 + ms(500), host_seen(0));
        assert_eq!(link.on_rejoined(t0 + ms(900)), [LinkAction::ShowResumed]);
        assert!(!link.is_reconnecting() && !link.is_rebuilding());
    }

    #[test]
    fn a_viewer_rejoin_resumes_at_its_next_frame() {
        let t0 = Instant::now();
        let mut link = StreamLink::new(StreamRole::Viewer);
        link.on_dropped(t0);
        link.observe(t0 + ms(500), viewer_seen(0, None, false));
        assert!(link.on_rejoined(t0 + ms(900)).is_empty());
        assert!(link.is_reconnecting());
        assert!(link
            .observe(t0 + ms(950), viewer_seen(50, None, false))
            .is_empty());
        assert_eq!(
            link.observe(t0 + ms(1_100), viewer_seen(50, None, true)),
            [LinkAction::ShowResumed]
        );
    }

    /// A pulled cable on the host: the viewer's own link answers, the host
    /// media stops. The viewer shows "reconnecting" at 700 ms and resumes
    /// at the next frame, with no rejoin of its own.
    #[test]
    fn a_silent_host_shows_reconnecting_on_the_viewer_without_a_rejoin() {
        let t0 = Instant::now();
        let mut link = StreamLink::new(StreamRole::Viewer);
        assert!(link
            .observe(t0 + ms(600), viewer_seen(40, Some(600), false))
            .is_empty());
        assert_eq!(
            link.observe(t0 + ms(700), viewer_seen(40, Some(700), false)),
            [LinkAction::ShowReconnecting]
        );
        assert!(link
            .observe(t0 + ms(6_000), viewer_seen(40, Some(6_000), false))
            .is_empty());
        assert!(!link.is_rebuilding(), "the host's outage is not a rejoin");
        assert_eq!(
            link.observe(t0 + ms(6_100), viewer_seen(40, Some(0), true)),
            [LinkAction::ShowResumed]
        );
    }

    #[test]
    fn a_viewer_gives_up_after_30_s_without_a_frame() {
        let t0 = Instant::now();
        let mut link = StreamLink::new(StreamRole::Viewer);
        link.on_host_reconnecting(t0);
        assert!(link
            .observe(t0 + ms(29_000), viewer_seen(40, Some(29_000), false))
            .is_empty());
        assert_eq!(
            link.observe(t0 + ms(30_000), viewer_seen(40, Some(30_000), false)),
            [LinkAction::GiveUp]
        );
    }

    #[test]
    fn host_reconnecting_from_the_sfu_shows_reconnecting_once() {
        let t0 = Instant::now();
        let mut link = StreamLink::new(StreamRole::Viewer);
        assert_eq!(
            link.on_host_reconnecting(t0),
            [LinkAction::ShowReconnecting]
        );
        assert!(link.on_host_reconnecting(t0 + ms(10)).is_empty());
        assert!(!link.is_rebuilding());
    }
}
