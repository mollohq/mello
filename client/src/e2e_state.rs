//! Test-only state port for the e2e driver (`e2e` feature, never shipped).
//!
//! Serves the app's state as JSON on `127.0.0.1:$MELLO_E2E_STATE_PORT`, so a
//! driver can wait on real state instead of sleeping or reading pixels
//! (see plans/E2E-QA.md §5.1). Phase 0 prototype: plain `std::net`, no new
//! dependencies, GET only.
//!
//! - `GET /state`  — a snapshot read on the UI thread.
//! - `GET /events` — the last [`EVENT_TAIL`] core events, oldest first.
//! - `POST /fake-game` — `{"action":"start"|"end","game_id":"rocket-league",
//!   "ago_min":12}` feeds a game sensor event for a catalogue game into the
//!   core loop (`mello_core::e2e_game`). `ago_min` moves a start into the past.
//! - `POST /game-clip-key` — press the game clip key (F8) once. The poll loop
//!   takes the press as it takes the real key (`take_game_clip_keys`).
//! - `POST /stream-key` — press the stream key (F9) once. The poll loop
//!   takes the press as it takes the real key (`take_stream_keys`).
//! - `GET /disabled` — the controls on screen that declare
//!   `accessible-enabled: false`. Slint MCP drops a `false` value from its
//!   answer, so the driver cannot tell a disabled control from one that does
//!   not declare the property. This list tells it.
//!
//! Everything here only reads what the UI already shows. Lists hold at most
//! [`LIST_LIMIT`] rows: the newest for messages, the first on screen for the
//! rest.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use i_slint_backend_testing::{AccessibleRole, ElementQuery};
use mello_core::events::GameClipState;
use mello_core::events::StreamMember;
use mello_core::{Event, VoiceMode};
use serde::Serialize;
use slint::{ComponentHandle, Model};

use crate::MainWindow;

pub const PORT_ENV: &str = "MELLO_E2E_STATE_PORT";
const EVENT_TAIL: usize = 200;
/// The most rows in one list of the snapshot.
const LIST_LIMIT: usize = 20;
/// The roles that the driver treats as controls (tools/mello-driver/src/slint.ts).
const CONTROL_ROLES: [AccessibleRole; 6] = [
    AccessibleRole::Button,
    AccessibleRole::Switch,
    AccessibleRole::Tab,
    AccessibleRole::Slider,
    AccessibleRole::Combobox,
    AccessibleRole::TextInput,
];

static EVENTS: Mutex<VecDeque<EventRecord>> = Mutex::new(VecDeque::new());
static EVENT_SEQ: Mutex<u64> = Mutex::new(0);
/// The transport of the current voice call, from the core's events.
static VOICE_TRANSPORT: Mutex<Option<&'static str>> = Mutex::new(None);
/// The other members of the watched SFU stream, from the core's events.
static STREAM_MEMBERS: Mutex<Vec<StreamMember>> = Mutex::new(Vec::new());
/// Game clip key presses from `POST /game-clip-key` that the poll loop has
/// not taken.
static GAME_CLIP_KEYS: AtomicU32 = AtomicU32::new(0);
/// Stream key presses from `POST /stream-key` that the poll loop has not
/// taken.
static STREAM_KEYS: AtomicU32 = AtomicU32::new(0);

#[derive(Serialize, Clone)]
struct EventRecord {
    seq: u64,
    ts_ms: u128,
    #[serde(rename = "type")]
    kind: String,
    /// Only for `Error`: the UI never shows these, so the driver must see them.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Only for the game clip events: why a clip failed or the replay is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// Only for `GameClipStateChanged`: `"recording"`, `"off"` and so on.
    #[serde(skip_serializing_if = "Option::is_none")]
    game_clip_state: Option<GameClipState>,
    /// Only for the clip events that name a clip.
    #[serde(skip_serializing_if = "Option::is_none")]
    clip_id: Option<String>,
    /// Only for `VoiceStateChanged`: `"sfu"` or `"disconnected"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<&'static str>,
    /// Only for `ConnectionStateChanged`: the realtime socket is up.
    #[serde(skip_serializing_if = "Option::is_none")]
    connected: Option<bool>,
    /// Only for `ConnectionStateChanged`: a reconnect attempt is in flight.
    #[serde(skip_serializing_if = "Option::is_none")]
    reconnecting: Option<bool>,
}

#[derive(Serialize)]
struct Snapshot {
    screen: &'static str,
    onboarding_step: i32,
    logged_in: bool,
    show_sign_in: bool,
    user_id: String,
    user_name: String,
    login_error: String,
    /// Onboarding step 3: why linking an identity failed.
    link_error: String,
    /// A sign-in or link is in progress (the spinner shows).
    login_loading: bool,
    /// Onboarding step 2 and the welcome screen: the crew of the invite link.
    onboarding_invite_crew_name: String,
    /// The welcome screen: who made the invite. Empty when there is none.
    onboarding_invite_inviter: String,
    /// The welcome screen (onboarding step 5) is on screen.
    invite_welcome: bool,
    /// Onboarding skipped step 1 for an invite: two steps, not three.
    onboarding_invite_path: bool,
    /// The join modal: who made the invite. Empty when there is none.
    join_crew_inviter: String,
    /// Onboarding step 1: why the invite link could not be used.
    onboarding_invite_error: String,
    /// The invite-code card on step 1: why the typed text was refused.
    onboarding_invite_code_error: String,
    /// The invite-code card on step 1: a resolve is running.
    onboarding_invite_code_checking: bool,
    /// The welcome screen and step 2: the invited crew shows its avatar.
    onboarding_invite_crew_has_avatar: bool,
    /// Onboarding step 2: the avatar cards that show their image, in grid order.
    avatar_loaded: [bool; 7],
    /// Onboarding step 2: the chosen avatar. 0 to 6 is a card in the grid,
    /// 7 is an upload, -1 is none.
    selected_avatar: i32,
    active_crew_id: String,
    active_crew_name: String,
    crews: Vec<String>,
    /// The quest gifts of the active crew (crew quests).
    crew_gifts: CrewGiftsSnap,
    members: Vec<String>,
    in_voice: bool,
    join_crew_modal_open: bool,
    join_crew_name: String,
    join_crew_error: String,
    mic_muted: bool,
    deafened: bool,
    /// The microphone permission that the control bar shows: `"granted"`,
    /// `"denied"` or `"undetermined"`. See [`mic_permission_name`].
    mic_permission: &'static str,
    /// Modals that are open, by name. The driver waits on these to close.
    open_modals: Vec<&'static str>,
    /// The last 20 chat messages in the active crew, oldest first.
    messages: Vec<MessageSnap>,
    voice_channels: Vec<VoiceChannelSnap>,
    /// The transport of the current voice call: `"sfu"`. `None` (JSON
    /// `null`) when not in a call. See [`transport_after`].
    voice_transport: Option<&'static str>,
    /// The other members of the watched SFU stream: the host and the other
    /// viewers, in join order. Empty when not watching.
    /// See [`stream_members_after`].
    stream_members: Vec<StreamMemberSnap>,
    /// Onboarding step 1: why the crew list did not load. Empty when it loaded.
    discover_error: String,
    /// Onboarding step 1: the crew cards, in screen order.
    onboarding_crews: Vec<CrewCardSnap>,
    /// The Discover panel: the crew cards, in screen order.
    discover_crews: Vec<CrewCardSnap>,
    /// The Discover panel: the number of crews in its list. It can be more
    /// than `discover_crews` holds.
    discover_crews_count: usize,
    /// The banner text when the connection to the backend is lost. Empty
    /// when connected.
    connection_warning: String,
    /// The crew settings modal: the values in its fields.
    crew_settings: CrewSettingsSnap,
    /// The member list of the active crew, in screen order.
    crew_members: Vec<MemberSnap>,
    stream: StreamSnap,
    /// The zone feed of the active crew (`handlers/zones.rs`).
    feed_zones: ZonesSnap,
    /// The clip that plays in the feed.
    clip: ClipSnap,
    /// The game clip player over the feed (CLIPS.md 15.9).
    game_clip_player: GameClipPlayerSnap,
    /// The stats strip: the viewer's last game with its square icon.
    you_strip: YouStripSnap,
    /// The live session cards of the Sessions zone, in screen order.
    live_sessions: Vec<LiveSessionSnap>,
    /// Sealed belts and recap in the feed, and the card in the centre.
    sealed: SealedSnap,
    /// The chat input has the cursor.
    chat_input_focused: bool,
    /// The audio page of the settings.
    audio: AudioSnap,
    riot: RiotSnap,
    /// The in-app update toast shows a new version.
    update_available: bool,
    /// The version the settings show, for example `"v0.0.0-DEV"`.
    app_version: String,
    last_event_seq: u64,
}

/// A crew card on onboarding step 1 or in the Discover panel.
#[derive(Serialize)]
struct CrewCardSnap {
    id: String,
    name: String,
    member_count: i32,
    /// 0 when the crew has no limit. The card is full when
    /// `member_count >= max_members` and `max_members > 0`.
    max_members: i32,
    /// Only in the Discover panel: anyone can join without an invite.
    #[serde(skip_serializing_if = "Option::is_none")]
    open: Option<bool>,
}

#[derive(Serialize)]
struct CrewSettingsSnap {
    name: String,
    description: String,
    /// The crew shows in Discover and anyone can join.
    open: bool,
    /// `"everyone"` or `"admins"`: who can make an invite code.
    invite_policy: String,
    /// `"owner"`, `"admin"` or `"member"`. See [`role_name`].
    my_role: &'static str,
    /// The "saved" confirmation shows.
    saved: bool,
}

#[derive(Serialize)]
struct MemberSnap {
    id: String,
    name: String,
    /// `"owner"`, `"admin"` or `"member"`. See [`role_name`].
    role: &'static str,
    online: bool,
    /// A fingerprint of the avatar image on screen. The same image gives the
    /// same key. Empty when the member shows initials. See [`image_key`].
    avatar_key: String,
}

#[derive(Serialize)]
struct StreamSnap {
    /// This user streams.
    hosting: bool,
    /// This user watches a stream.
    watching: bool,
    /// The size of the watched stream, from the stream metadata. 0 when unknown.
    width: i32,
    height: i32,
    viewer_count: i32,
    /// The quality pill: 0 Ultra, 1 High, 2 Medium, 3 Low, 4 Potato.
    preset: i32,
    /// The frames that the core gave to the viewer since the watch started.
    /// The core reports it once a second.
    frames_presented: i32,
    /// The live stream cards of the active crew, in screen order.
    cards: Vec<StreamCardSnap>,
}

#[derive(Serialize)]
struct StreamCardSnap {
    host_id: String,
    streamer_name: String,
    viewer_count: i32,
    width: i32,
    height: i32,
    can_watch: bool,
}

#[derive(Serialize)]
struct FeedCardSnap {
    id: String,
    card_type: String,
    title: String,
    actor_name: String,
    /// The local file of a clip card. Empty for other cards.
    clip_path: String,
    /// `"game"` for a game clip, else empty.
    clip_kind: String,
    /// The poster file of a game clip on this PC. Empty on other PCs.
    clip_poster_path: String,
    /// The poster is up and the video is not.
    clip_uploading: bool,
    /// The video is up, or this PC has the clip file.
    clip_can_play: bool,
    /// The poster is on the card.
    poster: bool,
    /// The card counts as new (unread).
    new: bool,
    game_name: String,
    /// The game art of the card: `"image"` or `"monogram"`.
    art: &'static str,
}

fn card_snap(c: &crate::FeedCardData) -> FeedCardSnap {
    FeedCardSnap {
        id: c.id.to_string(),
        card_type: c.card_type.to_string(),
        title: c.title.to_string(),
        actor_name: c.actor_name.to_string(),
        clip_path: c.clip_path.to_string(),
        clip_kind: c.clip_kind.to_string(),
        clip_poster_path: c.clip_poster_path.to_string(),
        clip_uploading: c.clip_uploading,
        clip_can_play: c.clip_can_play,
        poster: c.snapshot_poster_ready,
        new: c.is_new,
        game_name: c.game_name.to_string(),
        art: if c.game_has_icon || c.game_has_capsule {
            "image"
        } else {
            "monogram"
        },
    }
}

/// The zone feed (`ZonesData`), zone by zone.
#[derive(Serialize)]
struct ZonesSnap {
    /// `"live"`, `"quests"`, `"card"`, `"own"` or `""` (collapsed).
    stage: String,
    /// The quest card when `stage` is `"quests"` (crew quests).
    quests: Option<QuestsSnap>,
    /// The card on the Stage when `stage` is `"card"`.
    stage_card: Option<FeedCardSnap>,
    /// The belt hero, when the Stage card is one.
    belt_hero: Option<BeltHeroSnap>,
    /// The viewer's own chips (a crew of one): on the Stage, or on the solo
    /// board while the quest card holds the Stage.
    own_chips: Vec<OwnChipSnap>,
    /// The viewer's octagon wears an image.
    you_avatar: bool,
    /// `"sealed"`, `"tabs"`, `"quiet"`, `"past"`, `"solo"` or `""`.
    board: String,
    /// The belts of the board, in order.
    belts: Vec<BoardBeltSnap>,
    /// Every belt of the crew. The board draws seven at most.
    belt_count: i32,
    quiet_line: String,
    /// The Today tab of the board.
    today: TodaySnap,
    /// The games of a crew of one on the solo board.
    solo_games: Vec<String>,
    /// The dormant strip, if it shows.
    dormant: Option<DormantSnap>,
    clips: Vec<FeedCardSnap>,
    people: PeopleSnap,
    sessions: Vec<FeedCardSnap>,
    /// The games most members have, on a quiet week with no session.
    installed: Vec<String>,
    history: Vec<HistoryWeekSnap>,
    /// The server has another page of history.
    history_more: bool,
    /// The viewer has m3llo+.
    plus: bool,
}

#[derive(Serialize)]
struct OwnChipSnap {
    title: String,
    /// The value as the chip draws it, for example `"4,167"`.
    value: String,
    sealed: bool,
}

/// The Today tab of the board: the daily recap of the current day.
#[derive(Serialize)]
struct TodaySnap {
    /// Nothing today: the tab is dim.
    empty: bool,
    /// For example `"6 h 40"` or `"48 min"`.
    hangout: String,
    clips: i64,
    sessions: i64,
    /// The members who took part.
    on: i64,
    /// Empty when the day has no top game.
    top_game: String,
}

fn stat_text(s: &crate::BoardStat) -> String {
    [s.value.as_str(), s.unit.as_str(), s.tail.as_str()]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Serialize)]
struct BoardBeltSnap {
    title: String,
    holder_name: String,
    sealed: bool,
    unread: bool,
}

#[derive(Serialize)]
struct PeopleSnap {
    /// The People zone shows.
    shown: bool,
    /// The octagons of the first five, and how many wear an image.
    members: usize,
    members_with_avatar: usize,
    open_seats: i32,
    /// Who joined in the last 24 hours.
    joined: Vec<String>,
    joined_line: String,
    /// The Say hi button shows.
    say_hi: bool,
    /// `"call"`, `"idle"` or `""`.
    voice: String,
    channel: String,
}

#[derive(Serialize)]
struct HistoryWeekSnap {
    label: String,
    locked: bool,
    /// The cards of an open week.
    clips: usize,
    /// The teasers of a locked week, and the "+N" after them.
    teasers: usize,
    more: i32,
}

/// The quest card: the count, each gift and each step.
#[derive(Serialize)]
struct QuestsSnap {
    /// "2 / 5".
    count: String,
    gifts: Vec<QuestGiftSnap>,
    rows: Vec<QuestRowSnap>,
}

#[derive(Serialize)]
struct QuestGiftSnap {
    label: String,
    /// `"locked"`, `"ready"` or `"open"`.
    state: String,
    /// The viewer opens it (the owner, a ready gift).
    can_open: bool,
}

#[derive(Serialize)]
struct QuestRowSnap {
    name: String,
    done: bool,
    /// `"invite"`, `"voice"`, `"settings"` or `""`.
    action: String,
}

fn read_zones(app: &MainWindow) -> ZonesSnap {
    let z = app.get_feed_zones();
    let stage_card = (z.stage == "card").then(|| z.stage_card.clone());
    ZonesSnap {
        stage: z.stage.to_string(),
        quests: (z.stage == "quests").then(|| QuestsSnap {
            count: z.quest_count.to_string(),
            gifts: z
                .quest_gifts
                .iter()
                .map(|g| QuestGiftSnap {
                    label: g.label.to_string(),
                    state: g.state.to_string(),
                    can_open: g.can_open,
                })
                .collect(),
            rows: z
                .quest_rows
                .iter()
                .map(|r| QuestRowSnap {
                    name: r.name.to_string(),
                    done: r.done,
                    action: r.action.to_string(),
                })
                .collect(),
        }),
        stage_card: stage_card.as_ref().map(card_snap),
        belt_hero: stage_card
            .filter(|c| c.card_type == "head2head" && c.head2head.variant == "belt")
            .map(|c| BeltHeroSnap {
                headline: c.head2head.belt.headline.to_string(),
                detail: c.head2head.belt.detail.to_string(),
                age: c.head2head.belt.age.to_string(),
            }),
        own_chips: z
            .own_chips
            .iter()
            .map(|t| OwnChipSnap {
                title: t.title.to_string(),
                value: t.value.to_string(),
                sealed: t.belt.sealed,
            })
            .collect(),
        you_avatar: z.you.has_avatar,
        board: z.board.to_string(),
        belts: z
            .titles
            .iter()
            .map(|t| BoardBeltSnap {
                title: t.title.to_string(),
                holder_name: t.person.name.to_string(),
                sealed: t.belt.sealed,
                unread: t.unread,
            })
            .collect(),
        belt_count: z.title_count,
        quiet_line: z.quiet_line.to_string(),
        today: {
            let n = |label: &str| {
                z.today_stats
                    .iter()
                    .find(|s| s.label == label)
                    .and_then(|s| s.value.parse::<i64>().ok())
                    .unwrap_or(0)
            };
            TodaySnap {
                empty: z.today_empty,
                hangout: stat_text(&z.today_big),
                clips: n("CLIPS"),
                sessions: n("SESSIONS"),
                on: n("ON"),
                top_game: if z.today_has_top {
                    z.today_top.name.to_string()
                } else {
                    String::new()
                },
            }
        },
        solo_games: z.solo_games.iter().map(|g| g.name.to_string()).collect(),
        dormant: (!z.dormant_line.is_empty()).then(|| DormantSnap {
            text: z.dormant_line.to_string(),
            age: z.dormant_age.to_string(),
        }),
        clips: z
            .clips
            .iter()
            .take(LIST_LIMIT)
            .map(|c| card_snap(&c))
            .collect(),
        people: PeopleSnap {
            shown: z.people,
            members: z.members.row_count(),
            members_with_avatar: z.members.iter().filter(|m| m.has_avatar).count(),
            open_seats: z.open_seats,
            joined: z.joined.iter().map(|p| p.name.to_string()).collect(),
            joined_line: z.joined_line.to_string(),
            say_hi: !z.say_hi_card.is_empty() && !z.said_hi,
            voice: z.voice.to_string(),
            channel: z.channel.to_string(),
        },
        sessions: z
            .sessions
            .iter()
            .take(LIST_LIMIT)
            .map(|c| card_snap(&c))
            .collect(),
        installed: z.installed.iter().map(|g| g.name.to_string()).collect(),
        history: z
            .history
            .iter()
            .map(|w| HistoryWeekSnap {
                label: w.label.to_string(),
                locked: w.locked,
                clips: w.clips.row_count(),
                teasers: w.teasers.row_count(),
                more: w.more,
            })
            .collect(),
        history_more: z.history_more,
        plus: z.plus,
    }
}

#[derive(Serialize)]
struct ClipSnap {
    /// The file that plays. Empty when no clip plays.
    path: String,
    paused: bool,
    /// From 0.0 to 1.0.
    progress: f32,
    duration_ms: i32,
}

#[derive(Serialize)]
struct GameClipPlayerSnap {
    open: bool,
    position_ms: i32,
    duration_ms: i32,
    buffering: bool,
    paused: bool,
    ended: bool,
    /// Why the clip cannot play. Empty when it plays.
    error: String,
}

#[derive(Serialize)]
struct DormantSnap {
    text: String,
    age: String,
}

#[derive(Serialize)]
struct BeltHeroSnap {
    headline: String,
    detail: String,
    age: String,
}

/// The sealed cards of the feed (spec `SEALED-REVEAL.md`).
#[derive(Serialize)]
struct SealedSnap {
    /// Different sealed belts in the feed. A belt in two slots counts once. The
    /// belt in the centre does not count.
    belts: usize,
    /// The weekly recap is sealed.
    recap: bool,
    /// Different sealed own chips of a crew of one (the day 0 reveal). They do
    /// not count in `belts`.
    own: usize,
    /// The card in the centre of the feed: `"belt"`, `"recap"` or `"none"`.
    open_center: &'static str,
    /// The card in the centre is at rest: the face shows, and a click on the
    /// scrim closes it. The reveal takes 1.25 s.
    resting: bool,
}

/// Count the sealed belts and the sealed recap of the zones.
fn read_sealed(app: &MainWindow) -> SealedSnap {
    let ui = app.global::<crate::SealedUi>();
    let active = ui.get_active_key();
    let mut keys: Vec<String> = Vec::new();
    let mut own: Vec<String> = Vec::new();
    let mut recap = false;
    let mut add = |b: &crate::BeltReveal| {
        let key = b.key.to_string();
        let list = if b.own { &mut own } else { &mut keys };
        if b.sealed && b.key != active && !list.contains(&key) {
            list.push(key);
        }
    };
    let z = app.get_feed_zones();
    z.titles.iter().for_each(|t| add(&t.belt));
    z.own_chips.iter().for_each(|t| add(&t.belt));
    let r = &z.recap_card;
    r.belts_moved.iter().for_each(|b| add(&b.belt));
    recap |= r.recap_seal.sealed && r.recap_seal.key != active;
    SealedSnap {
        belts: keys.len(),
        own: own.len(),
        recap,
        open_center: match ui.get_active_kind().as_str() {
            "belt" => "belt",
            "recap" => "recap",
            "quest" => "quest",
            _ => "none",
        },
        resting: ui.get_resting(),
    }
}

/// A live session card (a game in progress).
#[derive(Serialize)]
struct LiveSessionSnap {
    game_name: String,
    /// Every player, the viewer first.
    member_names: Vec<String>,
    since_min: i32,
    since_text: String,
    /// The place in the Sessions zone. 0 is the first card.
    index: usize,
    /// The octagons drawn, and how many wear an avatar.
    octagons: usize,
    with_avatar: usize,
    /// The card has local poster art.
    poster: bool,
}

/// The stats strip of the feed, the viewer's own.
#[derive(Serialize)]
struct YouStripSnap {
    has_stats: bool,
    game_name: String,
    /// `"image"` (icon slot art) or `"monogram"`.
    art: &'static str,
}

#[derive(Serialize)]
struct AudioSnap {
    /// The device names that the settings show.
    capture_device: String,
    playback_device: String,
    /// Push to talk is on (voice activity is off).
    push_to_talk: bool,
    /// The key that the settings show for push to talk.
    push_to_talk_key: String,
}

#[derive(Serialize)]
struct RiotSnap {
    linked: bool,
    /// The Riot ID that the settings show. Empty when not linked.
    id: String,
}

/// A control that declares `accessible-enabled: false` (`GET /disabled`).
/// The driver matches it to a Slint MCP control by role, label and place.
#[derive(Serialize, Debug, PartialEq)]
struct DisabledControl {
    role: String,
    label: String,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Serialize)]
struct StreamMemberSnap {
    user_id: String,
    /// `"host"` or `"viewer"`.
    role: String,
}

#[derive(Serialize)]
struct MessageSnap {
    sender: String,
    text: String,
    id: String,
    /// The message this one replies to. Empty when it is not a reply.
    reply_to_name: String,
    reply_preview: String,
    edited: bool,
    deleted: bool,
    gif: bool,
    /// How many images the message carries.
    images: usize,
    /// How many distinct reaction emoji the message has.
    reactions: usize,
    /// How many of those reactions draw a crew emoji image.
    image_reactions: usize,
    /// The message is only crew emoji, drawn as big images: how many.
    crew_emoji: usize,
    /// The message mentions this user (the row is marked).
    mentions_me: bool,
}

/// The quest gifts of the active crew: the banner, the custom invite name
/// and the crew emoji.
#[derive(Serialize)]
struct CrewGiftsSnap {
    /// The art Worker id of a game art banner. Empty for none or an upload.
    banner_game: String,
    /// The crew has an uploaded banner.
    banner_upload: bool,
    /// The crew header draws the banner image.
    banner_shown: bool,
    /// The custom invite name. Empty when the crew has none.
    invite_name: String,
    /// The crew emoji names, oldest first.
    emoji: Vec<String>,
    /// The crew emoji whose image is loaded, oldest first.
    emoji_loaded: Vec<String>,
    /// The emoji gift is open for this crew: the picker offers free slots.
    emoji_open: bool,
}

#[derive(Serialize)]
struct VoiceChannelSnap {
    name: String,
    active: bool,
    members: Vec<VoiceMemberSnap>,
}

#[derive(Serialize)]
struct VoiceMemberSnap {
    /// The user id. The member volume popover is keyed by it.
    id: String,
    name: String,
    speaking: bool,
    /// The member muted their own mic (server state).
    muted: bool,
    deafened: bool,
    /// The playback volume of this member for this user, 0 to 200 (%).
    volume: i32,
    /// This user muted the member for themselves only.
    local_muted: bool,
}

/// The microphone permission, as the state port reports it, from the two UI
/// properties that `MicPermissionChanged` sets.
fn mic_permission_name(granted: bool, denied: bool) -> &'static str {
    if granted {
        "granted"
    } else if denied {
        "denied"
    } else {
        "undetermined"
    }
}

/// The name of a crew role, as the crew settings show it. Nakama group
/// states: 0 superadmin (the owner), 1 admin, 2 member, 3 join request.
fn role_name(role: i32) -> &'static str {
    match role {
        0 => "owner",
        1 => "admin",
        2 => "member",
        3 => "join_request",
        _ => "unknown",
    }
}

/// A fingerprint of an image: its size and an FNV-1a hash of its pixels.
/// Empty for an image with no pixels (the default image).
fn image_key(img: &slint::Image) -> String {
    let size = img.size();
    if size.width == 0 || size.height == 0 {
        return String::new();
    }
    let Some(buf) = img.to_rgba8() else {
        return img
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("{}x{}", size.width, size.height));
    };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in buf.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{}x{}-{hash:016x}", size.width, size.height)
}

/// The first [`LIST_LIMIT`] rows of a model, converted.
fn first_rows<T: Clone + 'static, U>(model: &slint::ModelRc<T>, f: impl Fn(&T) -> U) -> Vec<U> {
    model.iter().take(LIST_LIMIT).map(|r| f(&r)).collect()
}

/// The name of a transport, as the state port reports it.
fn transport_name(mode: VoiceMode) -> &'static str {
    match mode {
        VoiceMode::SFU => "sfu",
        VoiceMode::Disconnected => "disconnected",
    }
}

/// The transport of the current voice call after `ev`, from `prev`.
///
/// The core reports the transport in `VoiceStateChanged` when a call starts
/// on the SFU, and when the call ends or its SFU join failed. `VoiceSfuDisconnected` means the SFU call dropped: there is no
/// transport until the core joins again. A channel switch keeps the old value
/// until the new call starts: the core reports no event when it stops the old
/// media. The driver therefore waits on the first `VoiceStateChanged` after
/// the `VoiceJoined` of the channel (tools/mello-driver/src/voice.ts).
fn transport_after(prev: Option<&'static str>, ev: &Event) -> Option<&'static str> {
    match ev {
        Event::VoiceStateChanged { transport, .. } => match transport {
            VoiceMode::Disconnected => None,
            mode => Some(transport_name(*mode)),
        },
        Event::VoiceSfuDisconnected { .. } => None,
        _ => prev,
    }
}

/// The stream members after `ev`, from `prev`. `StreamMembersChanged`
/// carries the whole list; the end of watching clears it.
fn stream_members_after(prev: Vec<StreamMember>, ev: &Event) -> Vec<StreamMember> {
    match ev {
        Event::StreamMembersChanged { members } => members.clone(),
        Event::StreamWatchingStopped => Vec::new(),
        _ => prev,
    }
}

/// Record a core event before the UI handles it. Called from the poll loop.
pub fn record_event(ev: &Event) {
    {
        let mut t = VOICE_TRANSPORT.lock().expect("voice transport lock");
        *t = transport_after(*t, ev);
    }
    {
        let mut m = STREAM_MEMBERS.lock().expect("stream members lock");
        *m = stream_members_after(std::mem::take(&mut *m), ev);
    }
    let seq = {
        let mut s = EVENT_SEQ.lock().expect("event seq lock");
        *s += 1;
        *s
    };
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let rec = event_record(ev, seq, ts_ms);
    let mut q = EVENTS.lock().expect("event tail lock");
    if q.len() == EVENT_TAIL {
        q.pop_front();
    }
    q.push_back(rec);
}

/// The record of one core event in the `/events` tail.
fn event_record(ev: &Event, seq: u64, ts_ms: u128) -> EventRecord {
    let message = match ev {
        Event::Error { message } => Some(message.clone()),
        _ => None,
    };
    let reason = match ev {
        Event::GameClipStateChanged { reason, .. }
        | Event::GameClipFailed { reason }
        | Event::GameClipUploadFailed { reason, .. }
        | Event::GameClipPlaybackClosed { reason } => Some(reason.clone()),
        _ => None,
    }
    .filter(|r| !r.is_empty());
    let game_clip_state = match ev {
        Event::GameClipStateChanged { state, .. } => Some(*state),
        _ => None,
    };
    let clip_id = match ev {
        Event::GameClipCaptured { clip_id, .. }
        | Event::GameClipUploadFailed { clip_id, .. }
        | Event::ClipPosted { clip_id, .. }
        | Event::ClipUploaded { clip_id, .. } => Some(clip_id.clone()),
        _ => None,
    };
    let transport = match ev {
        Event::VoiceStateChanged { transport, .. } => Some(transport_name(*transport)),
        _ => None,
    };
    let (connected, reconnecting) = match ev {
        Event::ConnectionStateChanged {
            connected,
            reconnecting,
        } => (Some(*connected), Some(*reconnecting)),
        _ => (None, None),
    };
    EventRecord {
        seq,
        ts_ms,
        kind: perf_scenarios::event_type(ev).to_string(),
        message,
        reason,
        game_clip_state,
        clip_id,
        transport,
        connected,
        reconnecting,
    }
}

/// The game clip key presses that `POST /game-clip-key` made since the last
/// call. Called from the poll loop.
pub fn take_game_clip_keys() -> u32 {
    GAME_CLIP_KEYS.swap(0, Ordering::Relaxed)
}

/// The stream key presses that `POST /stream-key` made since the last call.
/// Called from the poll loop.
pub fn take_stream_keys() -> u32 {
    STREAM_KEYS.swap(0, Ordering::Relaxed)
}

/// Start the port when `MELLO_E2E_STATE_PORT` is set. A no-op otherwise.
pub fn start(app: &MainWindow) {
    let Ok(port) = std::env::var(PORT_ENV) else {
        return;
    };
    let listener = match TcpListener::bind(format!("127.0.0.1:{port}")) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("[e2e] state port {port} failed to bind: {e}");
            return;
        }
    };
    log::info!("[e2e] state port listening on http://127.0.0.1:{port}/state");
    let weak = app.as_weak();
    std::thread::Builder::new()
        .name("e2e-state".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &weak);
            }
        })
        .expect("spawn e2e state thread");
}

fn serve(mut stream: TcpStream, weak: &slint::Weak<MainWindow>) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    // Headers up to the blank line, then a body of `Content-Length` bytes.
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0).min(4096);
            }
        }
    }
    let mut body_bytes = vec![0u8; length];
    if std::io::Read::read_exact(&mut reader, &mut body_bytes).is_err() {
        body_bytes.clear();
    }
    let method = line.split_whitespace().next().unwrap_or("GET");
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let (status, body) = match path {
        "/fake-game" if method == "POST" => fake_game(&body_bytes),
        "/game-clip-key" if method == "POST" => {
            GAME_CLIP_KEYS.fetch_add(1, Ordering::Relaxed);
            ("200 OK", r#"{"ok":true}"#.into())
        }
        "/stream-key" if method == "POST" => {
            STREAM_KEYS.fetch_add(1, Ordering::Relaxed);
            ("200 OK", r#"{"ok":true}"#.into())
        }
        "/state" => match snapshot(weak) {
            Some(json) => ("200 OK", json),
            None => (
                "503 Service Unavailable",
                r#"{"error":"ui thread busy"}"#.into(),
            ),
        },
        "/events" => {
            let q = EVENTS.lock().expect("event tail lock");
            let all: Vec<EventRecord> = q.iter().cloned().collect();
            ("200 OK", serde_json::to_string(&all).unwrap_or_default())
        }
        "/disabled" => match on_ui_thread(weak, |app| {
            serde_json::to_string(&disabled_controls(app)).unwrap_or_default()
        }) {
            Some(json) => ("200 OK", json),
            None => (
                "503 Service Unavailable",
                r#"{"error":"ui thread busy"}"#.into(),
            ),
        },
        _ => ("404 Not Found", r#"{"error":"not found"}"#.into()),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// `POST /fake-game`: queue a sensor event for a catalogue game.
fn fake_game(body: &[u8]) -> (&'static str, String) {
    #[derive(serde::Deserialize)]
    struct Req {
        action: String,
        game_id: String,
        #[serde(default)]
        ago_min: i64,
    }
    let Ok(req) = serde_json::from_slice::<Req>(body) else {
        return ("400 Bad Request", r#"{"error":"bad body"}"#.into());
    };
    let action = match req.action.as_str() {
        "start" => mello_core::e2e_game::Action::Start,
        "end" => mello_core::e2e_game::Action::End,
        _ => {
            return (
                "400 Bad Request",
                r#"{"error":"action is start or end"}"#.into(),
            )
        }
    };
    match mello_core::e2e_game::inject(action, &req.game_id, req.ago_min) {
        Ok(()) => ("200 OK", r#"{"ok":true}"#.into()),
        Err(e) => (
            "400 Bad Request",
            serde_json::json!({ "error": e }).to_string(),
        ),
    }
}

/// Read the snapshot on the UI thread: Slint components are not `Send`.
fn snapshot(weak: &slint::Weak<MainWindow>) -> Option<String> {
    on_ui_thread(weak, |app| {
        serde_json::to_string(&read(app)).unwrap_or_default()
    })
}

/// Run `f` on the UI thread and wait at most 2 s for its answer.
fn on_ui_thread(
    weak: &slint::Weak<MainWindow>,
    f: impl FnOnce(&MainWindow) -> String + Send + 'static,
) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    let weak = weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            let _ = tx.send(f(&app));
        }
    })
    .ok()?;
    // A Slint MCP query holds the UI thread for as long as it walks the element
    // tree: about 14 s for a crew of 50 members. The state port waits it out.
    rx.recv_timeout(Duration::from_secs(30)).ok()
}

/// The controls on screen that declare `accessible-enabled: false`.
fn disabled_controls(app: &MainWindow) -> Vec<DisabledControl> {
    ElementQuery::from_root(app)
        .match_descendants()
        .match_predicate(|e| e.accessible_enabled() == Some(false))
        .find_all()
        .into_iter()
        .filter_map(|e| {
            let role = e.accessible_role()?;
            if !CONTROL_ROLES.contains(&role) {
                return None;
            }
            let pos = e.absolute_position();
            let size = e.size();
            Some(DisabledControl {
                role: format!("{role:?}"),
                label: e.accessible_label().unwrap_or_default().to_string(),
                x: pos.x,
                y: pos.y,
                width: size.width,
                height: size.height,
            })
        })
        .collect()
}

fn read(app: &MainWindow) -> Snapshot {
    let step = app.get_onboarding_step();
    let logged_in = app.get_logged_in();
    let show_sign_in = app.get_show_sign_in();
    let onboarding_state = crate::onboarding::OnboardingState::from_step(step);
    // main.slint: steps 1..=3 and 5, the states that render without a session.
    let onboarding_screen = onboarding_state.renders_without_session();
    // Mirrors the branch conditions in main.slint.
    let screen = if show_sign_in {
        "sign_in"
    } else if onboarding_screen {
        "onboarding"
    } else if logged_in {
        "app"
    } else {
        "blank"
    };
    let crews = app.get_crews();
    let members = app.get_members();
    Snapshot {
        screen,
        onboarding_step: step,
        logged_in,
        show_sign_in,
        user_id: app.get_user_id().into(),
        user_name: app.get_user_name().into(),
        login_error: app.get_login_error().into(),
        link_error: app.get_link_error().into(),
        login_loading: app.get_login_loading(),
        onboarding_invite_crew_name: app.get_onboarding_invite_crew_name().into(),
        onboarding_invite_inviter: app.get_onboarding_invite_inviter().name.into(),
        invite_welcome: !show_sign_in
            && onboarding_state == crate::onboarding::OnboardingState::InviteWelcome,
        onboarding_invite_path: app.get_onboarding_invite_path(),
        join_crew_inviter: app.get_join_crew_inviter().name.into(),
        onboarding_invite_error: app.get_onboarding_invite_error().into(),
        onboarding_invite_code_error: app.get_onboarding_invite_code_error().into(),
        onboarding_invite_code_checking: app.get_onboarding_invite_code_checking(),
        onboarding_invite_crew_has_avatar: app.get_onboarding_invite_crew_has_avatar(),
        avatar_loaded: [
            app.get_avatar_loaded_0(),
            app.get_avatar_loaded_1(),
            app.get_avatar_loaded_2(),
            app.get_avatar_loaded_3(),
            app.get_avatar_loaded_4(),
            app.get_avatar_loaded_5(),
            app.get_avatar_loaded_6(),
        ],
        selected_avatar: app.get_selected_avatar(),
        active_crew_id: app.get_active_crew_id().into(),
        active_crew_name: app.get_active_crew_name().into(),
        crews: crews.iter().map(|c| c.name.to_string()).collect(),
        crew_gifts: {
            let active = app.get_active_crew_id();
            let c = crews.iter().find(|c| c.id == active).unwrap_or_default();
            let eu = app.global::<crate::CrewEmojiUi>();
            let emoji = eu.get_emoji();
            CrewGiftsSnap {
                banner_game: c.banner_game.to_string(),
                banner_upload: c.banner_game.is_empty() && !c.banner_url.is_empty(),
                banner_shown: c.has_banner,
                invite_name: c.invite_name.to_string(),
                emoji: emoji.iter().map(|e| e.name.to_string()).collect(),
                emoji_loaded: emoji
                    .iter()
                    .filter(|e| e.has_image)
                    .map(|e| e.name.to_string())
                    .collect(),
                emoji_open: eu.get_enabled(),
            }
        },
        members: members.iter().map(|m| m.name.to_string()).collect(),
        in_voice: app.get_in_voice(),
        join_crew_modal_open: app.get_join_crew_modal_open(),
        join_crew_name: app.get_join_crew_name().into(),
        join_crew_error: app.get_join_crew_error().into(),
        mic_muted: app.get_mic_muted(),
        mic_permission: mic_permission_name(
            app.get_mic_permission_granted(),
            app.get_mic_permission_denied(),
        ),
        open_modals: [
            ("settings", app.get_settings_open()),
            ("crew_settings", app.get_crew_settings_open()),
            ("new_crew", app.get_new_crew_open()),
            ("join_crew", app.get_join_crew_modal_open()),
            ("invite_share", app.get_invite_share_open()),
            ("stats_profile", app.get_stats_profile_open()),
            ("source_picker", app.get_source_picker_open()),
            ("source_menu", app.get_source_menu_open()),
            ("riot_link", app.get_riot_dialog_open()),
            ("discover", app.get_show_discover()),
            (
                "banner_picker",
                app.global::<crate::BannerPicker>().get_open(),
            ),
            (
                "invite_name",
                app.global::<crate::InviteNamePicker>().get_open(),
            ),
            ("crew_emoji", app.global::<crate::CrewEmojiUi>().get_open()),
        ]
        .into_iter()
        .filter_map(|(name, open)| open.then_some(name))
        .collect(),
        deafened: app.get_deafened(),
        messages: {
            let all: Vec<MessageSnap> = app
                .get_messages()
                .iter()
                .filter(|m| !m.is_system && !m.is_unread_divider)
                .map(|m| MessageSnap {
                    sender: m.sender_name.to_string(),
                    text: m.text.to_string(),
                    id: m.message_id.to_string(),
                    reply_to_name: m.reply_to_name.to_string(),
                    reply_preview: m.reply_preview_text.to_string(),
                    edited: m.is_edited,
                    deleted: m.is_deleted,
                    gif: m.is_gif,
                    images: m.images.row_count(),
                    reactions: m.reactions.row_count(),
                    image_reactions: m.reactions.iter().filter(|r| r.has_image).count(),
                    crew_emoji: m.crew_emoji.row_count(),
                    mentions_me: m.mentions_self,
                })
                .collect();
            let skip = all.len().saturating_sub(LIST_LIMIT);
            all.into_iter().skip(skip).collect()
        },
        voice_channels: app
            .get_voice_channels()
            .iter()
            .map(|c| VoiceChannelSnap {
                name: c.name.to_string(),
                active: c.active,
                members: c
                    .members
                    .iter()
                    .map(|m| VoiceMemberSnap {
                        id: m.id.to_string(),
                        name: m.name.to_string(),
                        speaking: m.speaking,
                        muted: m.muted,
                        deafened: m.deafened,
                        volume: m.volume,
                        local_muted: m.local_muted,
                    })
                    .collect(),
            })
            .collect(),
        voice_transport: *VOICE_TRANSPORT.lock().expect("voice transport lock"),
        stream_members: STREAM_MEMBERS
            .lock()
            .expect("stream members lock")
            .iter()
            .map(|m| StreamMemberSnap {
                user_id: m.user_id.clone(),
                role: m.role.clone(),
            })
            .collect(),
        discover_error: app.get_discover_error().into(),
        onboarding_crews: first_rows(&app.get_discover_crews(), |c| CrewCardSnap {
            id: c.id.to_string(),
            name: c.name.to_string(),
            member_count: c.member_count,
            max_members: c.max_members,
            open: None,
        }),
        discover_crews: first_rows(&app.get_discover_crews_list(), |c| CrewCardSnap {
            id: c.id.to_string(),
            name: c.name.to_string(),
            member_count: c.member_count,
            max_members: c.max_members,
            open: Some(c.open),
        }),
        discover_crews_count: app.get_discover_crews_list().row_count(),
        connection_warning: app.get_connection_warning().into(),
        crew_settings: CrewSettingsSnap {
            name: app.get_crew_settings_name().into(),
            description: app.get_crew_settings_description().into(),
            open: app.get_crew_settings_is_open(),
            invite_policy: app.get_crew_settings_invite_policy().into(),
            my_role: role_name(app.get_crew_settings_my_role()),
            saved: app.get_crew_settings_show_saved(),
        },
        crew_members: first_rows(&members, |m| MemberSnap {
            id: m.id.to_string(),
            name: m.name.to_string(),
            role: role_name(m.role),
            online: m.online,
            avatar_key: if m.has_avatar {
                image_key(&m.avatar)
            } else {
                String::new()
            },
        }),
        stream: StreamSnap {
            hosting: app.get_is_hosting(),
            watching: app.get_is_watching(),
            width: app.get_active_stream_width(),
            height: app.get_active_stream_height(),
            viewer_count: app.get_active_stream_viewer_count(),
            preset: app.get_stream_preset(),
            frames_presented: app.get_dbg_stream_frames(),
            cards: first_rows(&app.get_active_stream_cards(), |c| StreamCardSnap {
                host_id: c.host_id.to_string(),
                streamer_name: c.streamer_name.to_string(),
                viewer_count: c.viewer_count,
                width: c.stream_width,
                height: c.stream_height,
                can_watch: c.can_watch,
            }),
        },
        feed_zones: read_zones(app),
        clip: ClipSnap {
            path: app.get_clip_playing_path().into(),
            paused: app.get_clip_paused(),
            progress: app.get_clip_progress(),
            duration_ms: app.get_clip_duration_ms(),
        },
        game_clip_player: {
            let ui = app.global::<crate::ClipPlayerUi>();
            GameClipPlayerSnap {
                open: ui.get_open(),
                position_ms: ui.get_position_ms(),
                duration_ms: ui.get_duration_ms(),
                buffering: ui.get_buffering(),
                paused: ui.get_paused(),
                ended: ui.get_ended(),
                error: ui.get_error().into(),
            }
        },
        sealed: read_sealed(app),
        you_strip: {
            let strip = app.get_you_strip();
            YouStripSnap {
                has_stats: strip.has_stats,
                game_name: strip.game_name.to_string(),
                art: if strip.has_icon { "image" } else { "monogram" },
            }
        },
        live_sessions: app
            .get_feed_zones()
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, c)| c.card_type == "live-session")
            .map(|(index, c)| LiveSessionSnap {
                game_name: c.game_name.to_string(),
                member_names: c.live.member_names.iter().map(|n| n.to_string()).collect(),
                since_min: c.live.since_min,
                since_text: c.live.since_text.to_string(),
                index,
                octagons: c.live.players.row_count(),
                with_avatar: c.live.players.iter().filter(|p| p.has_avatar).count(),
                poster: c.live.has_poster,
            })
            .collect(),
        chat_input_focused: app.global::<crate::ChatFocus>().get_input_focused(),
        audio: AudioSnap {
            capture_device: app.get_selected_capture_name().into(),
            playback_device: app.get_selected_playback_name().into(),
            push_to_talk: app.get_settings_ptt_mode(),
            push_to_talk_key: app.get_settings_ptt_key_label().into(),
        },
        riot: RiotSnap {
            linked: app.get_riot_linked(),
            id: app.get_riot_id().into(),
        },
        update_available: app.get_update_available(),
        app_version: app.get_settings_build_version().into(),
        last_event_seq: *EVENT_SEQ.lock().expect("event seq lock"),
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use serde_json::{json, Value};

    use super::*;
    use crate::testkit::Harness;

    fn model<T: Clone + 'static>(rows: Vec<T>) -> slint::ModelRc<T> {
        Rc::new(slint::VecModel::from(rows)).into()
    }

    fn snapshot_json(app: &MainWindow) -> Value {
        serde_json::to_value(read(app)).expect("snapshot serializes")
    }

    fn pixels(rgba: [u8; 4]) -> slint::Image {
        let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(2, 2);
        for p in buf.make_mut_slice() {
            *p = slint::Rgba8Pixel::new(rgba[0], rgba[1], rgba[2], rgba[3]);
        }
        slint::Image::from_rgba8(buf)
    }

    #[test]
    fn the_snapshot_reads_discover_crew_settings_and_members() {
        let h = Harness::new();
        let app = h.app();
        app.set_discover_error("Could not load crews".into());
        app.set_discover_crews(model(vec![crate::CrewData {
            id: "c1".into(),
            name: "Night Owls".into(),
            member_count: 10,
            max_members: 10,
            ..Default::default()
        }]));
        app.set_discover_crews_list(model(vec![crate::DiscoverCrewData {
            id: "c2".into(),
            name: "Early Birds".into(),
            member_count: 3,
            max_members: 50,
            open: true,
            ..Default::default()
        }]));
        app.set_connection_warning("Connection lost. Reconnecting…".into());
        app.set_crew_settings_name("Night Owls".into());
        app.set_crew_settings_description("We play late".into());
        app.set_crew_settings_is_open(false);
        app.set_crew_settings_invite_policy("admins".into());
        app.set_crew_settings_my_role(1);
        app.set_crew_settings_show_saved(true);
        app.set_members(model(vec![
            crate::MemberData {
                id: "u1".into(),
                name: "alice".into(),
                role: 0,
                online: true,
                has_avatar: true,
                avatar: pixels([1, 2, 3, 255]),
                ..Default::default()
            },
            crate::MemberData {
                id: "u2".into(),
                name: "bob".into(),
                role: 2,
                ..Default::default()
            },
        ]));

        let s = snapshot_json(app);
        assert_eq!(s["discover_error"], "Could not load crews");
        assert_eq!(
            s["onboarding_crews"],
            json!([{"id": "c1", "name": "Night Owls", "member_count": 10, "max_members": 10}])
        );
        assert_eq!(
            s["discover_crews"],
            json!([{"id": "c2", "name": "Early Birds", "member_count": 3, "max_members": 50, "open": true}])
        );
        assert_eq!(s["discover_crews_count"], 1);
        assert_eq!(s["connection_warning"], "Connection lost. Reconnecting…");
        assert_eq!(
            s["crew_settings"],
            json!({"name": "Night Owls", "description": "We play late", "open": false,
                   "invite_policy": "admins", "my_role": "admin", "saved": true})
        );
        let members = s["crew_members"].as_array().expect("crew_members");
        assert_eq!(members[0]["name"], "alice");
        assert_eq!(members[0]["role"], "owner");
        assert_eq!(members[0]["online"], true);
        assert_eq!(
            members[0]["avatar_key"],
            Value::String(image_key(&pixels([1, 2, 3, 255])))
        );
        assert_eq!(members[1]["role"], "member");
        assert_eq!(members[1]["avatar_key"], "", "initials, no image");
        assert_eq!(s["members"], json!(["alice", "bob"]), "the old field stays");
    }

    #[test]
    fn the_snapshot_reads_messages_streams_feed_and_settings() {
        let h = Harness::new();
        let app = h.app();
        app.set_messages(model(vec![crate::ChatMessageData {
            message_id: "m1".into(),
            sender_name: "bob".into(),
            text: "@alice yes".into(),
            reply_to_id: "m0".into(),
            reply_to_name: "alice".into(),
            reply_preview_text: "ready?".into(),
            is_edited: true,
            is_gif: true,
            mentions_self: true,
            ..Default::default()
        }]));
        app.set_is_hosting(true);
        app.set_is_watching(false);
        app.set_active_stream_width(1280);
        app.set_active_stream_height(720);
        app.set_active_stream_viewer_count(1);
        app.set_stream_preset(3);
        app.set_dbg_stream_frames(42);
        app.set_active_stream_cards(model(vec![crate::StreamCardData {
            host_id: "u1".into(),
            streamer_name: "alice".into(),
            viewer_count: 1,
            stream_width: 1280,
            stream_height: 720,
            can_watch: true,
            ..Default::default()
        }]));
        app.set_feed_zones(crate::ZonesData {
            clips: model(vec![crate::FeedCardData {
                id: "f1".into(),
                card_type: "clip".into(),
                title: "Clutch".into(),
                actor_name: "alice".into(),
                clip_path: "/clips/a.mp4".into(),
                clip_kind: "game".into(),
                clip_poster_path: "/clips/a.jpg".into(),
                clip_uploading: true,
                clip_can_play: true,
                is_new: true,
                ..Default::default()
            }]),
            ..Default::default()
        });
        app.set_clip_playing_path("/clips/a.mp4".into());
        app.set_clip_paused(true);
        app.set_clip_progress(0.5);
        app.set_clip_duration_ms(30_000);
        app.set_selected_capture_name("USB Mic".into());
        app.set_selected_playback_name("Speakers".into());
        app.set_settings_ptt_mode(true);
        app.set_settings_ptt_key_label("F13".into());
        app.set_riot_linked(true);
        app.set_riot_id("alice#EUW".into());
        app.set_update_available(true);
        app.set_settings_build_version("v1.2.3".into());

        let s = snapshot_json(app);
        assert_eq!(
            s["messages"],
            json!([{"sender": "bob", "text": "@alice yes", "id": "m1",
                    "reply_to_name": "alice", "reply_preview": "ready?",
                    "edited": true, "deleted": false, "gif": true, "mentions_me": true,
                    "images": 0, "reactions": 0}])
        );
        assert_eq!(
            s["stream"],
            json!({"hosting": true, "watching": false, "width": 1280, "height": 720,
                   "viewer_count": 1, "preset": 3, "frames_presented": 42,
                   "cards": [{"host_id": "u1", "streamer_name": "alice", "viewer_count": 1,
                              "width": 1280, "height": 720, "can_watch": true}]})
        );
        assert_eq!(
            s["feed_zones"]["clips"],
            json!([{"id": "f1", "card_type": "clip", "title": "Clutch",
                    "actor_name": "alice", "clip_path": "/clips/a.mp4",
                    "clip_kind": "game", "clip_poster_path": "/clips/a.jpg",
                    "clip_uploading": true, "clip_can_play": true,
                    "poster": false, "new": true, "game_name": "",
                    "art": "monogram"}])
        );
        assert_eq!(
            s["clip"],
            json!({"path": "/clips/a.mp4", "paused": true, "progress": 0.5, "duration_ms": 30000})
        );
        assert_eq!(
            s["game_clip_player"],
            json!({"open": false, "position_ms": 0, "duration_ms": 0, "buffering": false,
                   "paused": false, "ended": false, "error": ""})
        );
        let player = app.global::<crate::ClipPlayerUi>();
        player.set_open(true);
        player.set_position_ms(7_000);
        player.set_duration_ms(30_000);
        player.set_buffering(true);
        assert_eq!(
            snapshot_json(app)["game_clip_player"],
            json!({"open": true, "position_ms": 7000, "duration_ms": 30000, "buffering": true,
                   "paused": false, "ended": false, "error": ""})
        );
        assert_eq!(
            s["audio"],
            json!({"capture_device": "USB Mic", "playback_device": "Speakers",
                   "push_to_talk": true, "push_to_talk_key": "F13"})
        );
        assert_eq!(s["riot"], json!({"linked": true, "id": "alice#EUW"}));
        assert_eq!(s["update_available"], true);
        assert_eq!(s["app_version"], "v1.2.3");
    }

    #[test]
    fn the_snapshot_counts_sealed_belts_once_and_reports_the_centre_card() {
        let h = Harness::new();
        let app = h.app();
        assert_eq!(
            snapshot_json(app)["sealed"],
            json!({"belts": 0, "own": 0, "recap": false, "open_center": "none", "resting": false})
        );
        let belt = |key: &str, sealed: bool| crate::BeltReveal {
            key: key.into(),
            sealed,
            ..Default::default()
        };
        let title = |key: &str, sealed: bool| crate::BoardTitle {
            belt: belt(key, sealed),
            ..Default::default()
        };
        app.set_feed_zones(crate::ZonesData {
            titles: model(vec![
                title("night_owl:1", true),
                title("the_carry:2", false),
            ]),
            recap_card: crate::FeedCardData {
                card_type: "recap".into(),
                recap_seal: crate::RecapSeal {
                    key: "recap:7".into(),
                    sealed: true,
                    ..Default::default()
                },
                // The same belt as the board row: counted once.
                belts_moved: model(vec![crate::RecapBeltMovedRow {
                    belt: belt("night_owl:1", true),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            snapshot_json(app)["sealed"],
            json!({"belts": 1, "own": 0, "recap": true, "open_center": "none", "resting": false})
        );
        // The belt in the centre is no longer sealed in the feed.
        let ui = app.global::<crate::SealedUi>();
        ui.set_active_key("night_owl:1".into());
        ui.set_active_kind("belt".into());
        assert_eq!(
            snapshot_json(app)["sealed"],
            json!({"belts": 0, "own": 0, "recap": true, "open_center": "belt", "resting": false})
        );
        ui.set_active_kind("recap".into());
        assert_eq!(snapshot_json(app)["sealed"]["open_center"], "recap");
    }

    #[test]
    fn the_snapshot_reads_the_zones() {
        let h = Harness::new();
        let app = h.app();
        let none = snapshot_json(app)["feed_zones"].clone();
        assert_eq!(none["stage"], "");
        assert_eq!(none["stage_card"], Value::Null);
        assert_eq!(none["board"], "");
        assert_eq!(none["history"], json!([]));

        let person = |name: &str| crate::H2hPerson {
            name: name.into(),
            ..Default::default()
        };
        app.set_feed_zones(crate::ZonesData {
            stage: "card".into(),
            stage_card: crate::FeedCardData {
                id: "head2head-belt".into(),
                card_type: "head2head".into(),
                head2head: crate::Head2headData {
                    variant: "belt".into(),
                    belt: crate::BeltHeroData {
                        headline: "Sara took Night Owl from you".into(),
                        detail: "03:40 beats 03:14.".into(),
                        age: "6 days ago".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            board: "quiet".into(),
            quiet_line: "No crew time last week.".into(),
            today_big: crate::BoardStat {
                value: "6".into(),
                unit: "h".into(),
                tail: "40".into(),
                label: "HANGOUT".into(),
            },
            today_stats: model(vec![
                crate::BoardStat {
                    value: "2".into(),
                    label: "SESSIONS".into(),
                    ..Default::default()
                },
                crate::BoardStat {
                    value: "3".into(),
                    label: "ON".into(),
                    ..Default::default()
                },
            ]),
            today_has_top: true,
            today_top: crate::BoardGame {
                name: "Counter-Strike 2".into(),
                ..Default::default()
            },
            titles: model(vec![crate::BoardTitle {
                title: "NIGHT OWL".into(),
                person: person("Sara"),
                unread: true,
                ..Default::default()
            }]),
            title_count: 13,
            people: true,
            members: model(vec![person("You"), person("Sara")]),
            open_seats: 3,
            joined: model(vec![person("Jonas")]),
            joined_line: "Jonas joined".into(),
            say_hi_card: "joined-u2".into(),
            voice: "idle".into(),
            channel: "Lounge".into(),
            installed: model(vec![crate::BoardGame {
                name: "Deep Rock Galactic".into(),
                ..Default::default()
            }]),
            history: model(vec![crate::HistoryWeek {
                label: "Week 4".into(),
                locked: true,
                teasers: model(vec![crate::LockedTeaser::default()]),
                more: 5,
                ..Default::default()
            }]),
            history_more: true,
            ..Default::default()
        });
        let z = snapshot_json(app)["feed_zones"].clone();
        assert_eq!(z["stage"], "card");
        assert_eq!(z["stage_card"]["card_type"], "head2head");
        assert_eq!(
            z["belt_hero"],
            json!({"headline": "Sara took Night Owl from you", "detail": "03:40 beats 03:14.",
                   "age": "6 days ago"})
        );
        assert_eq!(z["board"], "quiet");
        assert_eq!(z["quiet_line"], "No crew time last week.");
        assert_eq!(
            z["today"],
            json!({"empty": false, "hangout": "6 h 40", "clips": 0, "sessions": 2, "on": 3,
                   "top_game": "Counter-Strike 2"})
        );
        assert_eq!(
            z["belts"],
            json!([{"title": "NIGHT OWL", "holder_name": "Sara", "sealed": false, "unread": true}])
        );
        assert_eq!(z["belt_count"], 13);
        assert_eq!(
            z["people"],
            json!({"shown": true, "members": 2, "members_with_avatar": 0, "open_seats": 3, "joined": ["Jonas"],
                   "joined_line": "Jonas joined", "say_hi": true, "voice": "idle",
                   "channel": "Lounge"})
        );
        assert_eq!(z["installed"], json!(["Deep Rock Galactic"]));
        assert_eq!(
            z["history"],
            json!([{"label": "Week 4", "locked": true, "clips": 0, "teasers": 1, "more": 5}])
        );
        assert_eq!(z["history_more"], true);
        assert_eq!(z["plus"], false);
    }

    /// The QA harness asserts the member volume and the local mute.
    #[test]
    fn the_snapshot_reads_the_member_volume_and_local_mute() {
        let h = Harness::new();
        let app = h.app();
        app.set_voice_channels(model(vec![crate::VoiceChannelData {
            id: "ch-1".into(),
            name: "General".into(),
            active: true,
            members: model(vec![
                crate::VoiceChannelMember {
                    id: "u-bob".into(),
                    name: "bob".into(),
                    muted: true,
                    deafened: true,
                    volume: 70,
                    ..Default::default()
                },
                crate::VoiceChannelMember {
                    id: "u-eve".into(),
                    name: "eve".into(),
                    volume: 100,
                    local_muted: true,
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }]));

        let s = snapshot_json(app);
        assert_eq!(
            s["voice_channels"][0]["members"],
            json!([
                {"id": "u-bob", "name": "bob", "speaking": false, "muted": true,
                 "deafened": true, "volume": 70, "local_muted": false},
                {"id": "u-eve", "name": "eve", "speaking": false, "muted": false,
                 "deafened": false, "volume": 100, "local_muted": true},
            ])
        );
    }

    #[test]
    fn lists_keep_the_newest_messages_and_the_first_crews() {
        let h = Harness::new();
        let app = h.app();
        let n = LIST_LIMIT + 5;
        app.set_messages(model(
            (0..n)
                .map(|i| crate::ChatMessageData {
                    message_id: format!("m{i}").into(),
                    text: format!("text {i}").into(),
                    ..Default::default()
                })
                .collect(),
        ));
        app.set_discover_crews_list(model(
            (0..n)
                .map(|i| crate::DiscoverCrewData {
                    name: format!("crew {i}").into(),
                    ..Default::default()
                })
                .collect(),
        ));
        let s = snapshot_json(app);
        let ids: Vec<&str> = s["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids.len(), LIST_LIMIT);
        assert_eq!(ids[0], "m5", "the newest, oldest first");
        assert_eq!(ids[LIST_LIMIT - 1], format!("m{}", n - 1));
        let crews = s["discover_crews"].as_array().expect("discover_crews");
        assert_eq!(crews.len(), LIST_LIMIT);
        assert_eq!(crews[0]["name"], "crew 0", "the first on screen");
        assert_eq!(s["discover_crews_count"], n);
    }

    #[test]
    fn the_avatar_key_is_the_same_for_the_same_pixels_only() {
        let a = image_key(&pixels([1, 2, 3, 255]));
        assert_eq!(a, image_key(&pixels([1, 2, 3, 255])));
        assert_ne!(a, image_key(&pixels([1, 2, 4, 255])));
        assert!(a.starts_with("2x2-"), "{a}");
        assert_eq!(image_key(&slint::Image::default()), "");
    }

    #[test]
    fn a_full_crew_card_is_reported_as_disabled() {
        let mut h = Harness::new();
        let crew = |name: &str, members: i32| mello_core::crew::Crew {
            id: name.into(),
            name: name.into(),
            description: String::new(),
            member_count: members,
            max_members: 10,
            open: true,
            avatar_url: None,
        };
        h.emit(Event::DiscoverCrewsLoaded {
            crews: vec![crew("Full House", 10), crew("Room Left", 3)],
            cursor: None,
        });
        let disabled = disabled_controls(h.app());
        let labels: Vec<&str> = disabled.iter().map(|d| d.label.as_str()).collect();
        assert!(labels.contains(&"Full House"), "{disabled:?}");
        assert!(!labels.contains(&"Room Left"), "{disabled:?}");
        let full = disabled
            .iter()
            .find(|d| d.label == "Full House")
            .expect("the full card");
        assert_eq!(full.role, "Button");
        assert!(full.width > 0.0 && full.height > 0.0, "{full:?}");
    }

    #[test]
    fn a_connection_event_records_its_state() {
        let rec = event_record(
            &Event::ConnectionStateChanged {
                connected: false,
                reconnecting: true,
            },
            7,
            1,
        );
        let v = serde_json::to_value(&rec).expect("record serializes");
        assert_eq!(
            v,
            json!({"seq": 7, "ts_ms": 1, "type": "ConnectionStateChanged",
                   "connected": false, "reconnecting": true})
        );
        let other = serde_json::to_value(event_record(&Event::MicLevel { level: 0.5 }, 8, 1))
            .expect("record serializes");
        assert!(other.get("connected").is_none(), "{other}");
    }

    #[test]
    fn a_game_clip_event_records_its_state_reason_and_clip() {
        let record = |ev: Event| {
            let mut v = serde_json::to_value(event_record(&ev, 1, 1)).expect("record serializes");
            v.as_object_mut()
                .expect("an object")
                .retain(|k, _| ["game_clip_state", "reason", "clip_id"].contains(&k.as_str()));
            v
        };
        assert_eq!(
            record(Event::GameClipStateChanged {
                state: GameClipState::Recording,
                game_name: "Rocket League".into(),
                reason: String::new(),
            }),
            json!({"game_clip_state": "recording"})
        );
        assert_eq!(
            record(Event::GameClipStateChanged {
                state: GameClipState::Unavailable,
                game_name: "Rocket League".into(),
                reason: "no hardware encoder".into(),
            }),
            json!({"game_clip_state": "unavailable", "reason": "no hardware encoder"})
        );
        assert_eq!(
            record(Event::GameClipCaptured {
                clip_id: "c1".into(),
                path: "/clips/c1.mp4".into(),
                poster_path: "/clips/c1.jpg".into(),
                duration_seconds: 30.0,
                width: 1280,
                height: 720,
                game_name: "Rocket League".into(),
            }),
            json!({"clip_id": "c1"})
        );
        assert_eq!(
            record(Event::GameClipUploadFailed {
                clip_id: "c1".into(),
                reason: "the files are gone".into(),
            }),
            json!({"clip_id": "c1", "reason": "the files are gone"})
        );
    }

    #[test]
    fn a_game_clip_key_post_is_taken_once() {
        let _ = take_game_clip_keys();
        GAME_CLIP_KEYS.fetch_add(2, Ordering::Relaxed);
        assert_eq!(take_game_clip_keys(), 2);
        assert_eq!(take_game_clip_keys(), 0);
    }

    #[test]
    fn a_stream_key_post_is_taken_once() {
        let _ = take_stream_keys();
        STREAM_KEYS.fetch_add(2, Ordering::Relaxed);
        assert_eq!(take_stream_keys(), 2);
        assert_eq!(take_stream_keys(), 0);
    }

    fn state_changed(in_call: bool, transport: VoiceMode) -> Event {
        Event::VoiceStateChanged { in_call, transport }
    }

    #[test]
    fn the_stream_members_follow_the_core_stream_events() {
        let host = StreamMember {
            user_id: "host-1".into(),
            role: "host".into(),
        };
        let viewer = StreamMember {
            user_id: "viewer-2".into(),
            role: "viewer".into(),
        };
        let both = stream_members_after(
            Vec::new(),
            &Event::StreamMembersChanged {
                members: vec![host.clone(), viewer.clone()],
            },
        );
        assert_eq!(both, [host.clone(), viewer]);
        let one = stream_members_after(
            both,
            &Event::StreamMembersChanged {
                members: vec![host.clone()],
            },
        );
        assert_eq!(
            one,
            std::slice::from_ref(&host),
            "the event carries the whole list"
        );
        let kept = stream_members_after(one, &Event::MicLevel { level: 0.5 });
        assert_eq!(kept, [host], "other events keep the list");
        assert!(
            stream_members_after(kept, &Event::StreamWatchingStopped).is_empty(),
            "the end of watching clears the list"
        );
    }

    #[test]
    fn the_mic_permission_has_one_name_for_each_state() {
        assert_eq!(mic_permission_name(true, false), "granted");
        assert_eq!(mic_permission_name(false, true), "denied");
        assert_eq!(mic_permission_name(false, false), "undetermined");
    }

    #[test]
    fn the_transport_follows_the_core_voice_events() {
        let sfu = transport_after(None, &state_changed(true, VoiceMode::SFU));
        assert_eq!(sfu, Some("sfu"));
        assert_eq!(
            transport_after(sfu, &state_changed(true, VoiceMode::Disconnected)),
            None,
            "voice capture did not start: no transport"
        );
        assert_eq!(
            transport_after(sfu, &state_changed(false, VoiceMode::Disconnected)),
            None,
            "the call ended"
        );
        assert_eq!(
            transport_after(
                sfu,
                &Event::VoiceSfuDisconnected {
                    crew_id: "c".into(),
                    reason: "liveness_timeout".into()
                }
            ),
            None,
            "the SFU call dropped"
        );
        assert_eq!(
            transport_after(sfu, &Event::MicLevel { level: 0.5 }),
            sfu,
            "other events keep the transport"
        );
    }
}
