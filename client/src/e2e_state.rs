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
use std::sync::{mpsc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use i_slint_backend_testing::{AccessibleRole, ElementQuery};
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

#[derive(Serialize, Clone)]
struct EventRecord {
    seq: u64,
    ts_ms: u128,
    #[serde(rename = "type")]
    kind: String,
    /// Only for `Error`: the UI never shows these, so the driver must see them.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
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
    /// The feed of the active crew, in screen order (newest first).
    feed_cards: Vec<FeedCardSnap>,
    /// The clip that plays in the feed.
    clip: ClipSnap,
    /// The head2head card of the feed (the first one), if any.
    head2head: Head2headSnap,
    /// The invite card of the feed, if any.
    invite_card: InviteCardSnap,
    /// The recent games card of the feed, if any.
    recent_games: RecentGamesSnap,
    /// The live session cards of the feed, in screen order.
    live_sessions: Vec<LiveSessionSnap>,
    /// The daily recap card of the feed, if any.
    daily_recap: DailyRecapSnap,
    /// Sealed belts and recap in the feed, and the card in the centre.
    sealed: SealedSnap,
    /// The skeleton trio cells that show (0 to 3).
    skeleton_trio: usize,
    /// The joined cards of the feed, in grid order. A join is the smallest card
    /// the grid has, a standard cell after the first row.
    joined: Vec<JoinedSnap>,
    /// The chat input has the cursor ("Say hi" on a joined card puts it there).
    chat_input_focused: bool,
    /// The quiet row (frame E): at most one, in the place of the recap.
    quiet_rows: Vec<JoinedRowSnap>,
    /// The dormant strip (frame F): the text and its age, if it shows.
    dormant_strip: Option<DormantSnap>,
    /// The belt hero leads the feed (frame E).
    belt_hero: Option<BeltHeroSnap>,
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

/// The sealed cards of the feed (plan `SEALED-REVEAL.md`).
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

/// Count the sealed belts and the sealed recap of the feed cards.
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
    for c in app.get_feed_cards().iter() {
        let h = &c.head2head;
        h.pairs.iter().for_each(|p| add(&p.chip.belt));
        h.chips.iter().for_each(|t| add(&t.belt));
        c.belts_moved.iter().for_each(|b| add(&b.belt));
        recap |= c.recap_seal.sealed && c.recap_seal.key != active;
    }
    SealedSnap {
        belts: keys.len(),
        own: own.len(),
        recap,
        open_center: match ui.get_active_kind().as_str() {
            "belt" => "belt",
            "recap" => "recap",
            _ => "none",
        },
        resting: ui.get_resting(),
    }
}

/// The head2head card of the feed. `present` is false when the feed has none;
/// then `placement` is `"none"` and the lists are empty.
#[derive(Serialize)]
struct Head2headSnap {
    present: bool,
    /// `"wide"`, `"cell"`, `"versus"` or `"none"`. A cell with no seats
    /// (`cell-full` on screen) is reported as `"cell"`.
    placement: &'static str,
    member_count: i32,
    open_seats: i32,
    /// The chips the card draws: the viewer's own values on the wide card.
    own_values: Vec<TitleChipSnap>,
    /// Every contested title and who holds it.
    titles: Vec<TitleHolderSnap>,
    /// Every pair of the viewer, drawn or not.
    pairs_count: usize,
    /// The pair rows the cell draws.
    pair_rows: usize,
    /// Pair rows that wear the red dot (a belt moved today, not yet seen).
    pair_rows_dot: usize,
    /// The "+N more" line of the cell. 0 when it does not show.
    more_pairs: i32,
    belt_moved_recent: bool,
    /// The viewer's octagon on the card: a fingerprint of the avatar image,
    /// "" when it shows initials (as `crew_members`).
    own_avatar_key: String,
    /// The opponent octagons of the pair rows, in row order. "" for initials.
    pair_avatar_keys: Vec<String>,
}

/// The daily recap card. `present` is false when the feed has none.
#[derive(Serialize)]
struct DailyRecapSnap {
    present: bool,
    /// "Today so far", "Yesterday" or a day name.
    day: String,
    /// The hangout time, for example `"2h 5m"`.
    hangout: String,
    clips: i32,
    sessions: i32,
    top_game: String,
    /// The member octagons drawn.
    members: usize,
    /// The place in the feed card list. 0 is the first card.
    index: usize,
    /// One entry for each belt: `"The Carry"`.
    belts: Vec<String>,
}

/// A live session card (a game in progress).
#[derive(Serialize)]
struct LiveSessionSnap {
    game_name: String,
    /// Every player, the viewer first.
    member_names: Vec<String>,
    since_min: i32,
    since_text: String,
    /// The place in the feed card list. 0 is the first card.
    index: usize,
    /// The octagons drawn, and how many wear an avatar.
    octagons: usize,
    with_avatar: usize,
    /// The card has local poster art and holds the wide slot. Without it the
    /// card is a cell.
    poster: bool,
}

/// The invite card of the feed. `present` is false when the feed has none.
#[derive(Serialize)]
struct InviteCardSnap {
    present: bool,
    /// The member octagons drawn.
    octagons: usize,
    /// The "+N" after the octagons. 0 when there is none.
    more_count: i32,
    /// The octagons that wear an image and not initials.
    with_avatar: usize,
}

#[derive(Serialize)]
struct TitleChipSnap {
    title_id: String,
    /// The number and its unit as the chip reads them, for example `"4.2 h"`.
    display: String,
}

#[derive(Serialize)]
struct TitleHolderSnap {
    title_id: String,
    /// The member list name of the holder. Empty when the holder is not in it.
    holder_name: String,
}

#[derive(Serialize)]
struct JoinedSnap {
    /// The joiner, for example `"Jonas"`.
    name: String,
    /// The headline, for example `"Jonas joined"`.
    title: String,
    /// For example `"invited by b0bben"`. Empty without an inviter.
    invited: String,
    /// For example `"Today 23:14"`.
    when: String,
    /// The card's place among the feed cards, from 0.
    index: usize,
}

#[derive(Serialize)]
struct JoinedRowSnap {
    /// For example `"Jonas joined, invited by b0bben"`.
    line: String,
    /// For example `"Today 23:14"`.
    when: String,
}

#[derive(Serialize)]
struct RecentGamesSnap {
    /// The right side of the card header: the week's line, for example
    /// `"14 sessions, 31 h this week"`. Empty when the card has none.
    header_line: String,
    /// `"grid"` (poster grid, two or more PCs) or `"list"`. Empty with no card.
    shape: &'static str,
    /// The PCs behind the tiles: the members who own or played a listed game.
    pc_count: i32,
    tiles: Vec<RecentGameTileSnap>,
}

#[derive(Serialize)]
struct RecentGameTileSnap {
    name: String,
    /// How many owner octagons the tile carries (the pips of the grid).
    owners: usize,
    /// The line under the name, for example `"4,167 H"` or `"EPIC"`.
    hours_text: String,
    /// Lower case: `"steam"`, `"epic"`, `"gog"`, or empty when unknown.
    launcher: String,
    tracked: bool,
    /// `"image"` (game art or icon) or `"monogram"` (the two-letter fallback).
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
    /// The message mentions this user (the row is marked).
    mentions_me: bool,
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

/// The placement name that the state port reports for a head2head variant.
fn placement_name(variant: &str) -> &'static str {
    match variant {
        "wide" => "wide",
        "versus" => "versus",
        "cell" | "cell-full" => "cell",
        _ => "none",
    }
}

/// The head2head card of the feed, read from the first `head2head` card.
fn read_head2head(app: &MainWindow) -> Head2headSnap {
    let feed = app.get_feed_cards();
    // The belt hero is a head2head card too, but the cell carries the pairs.
    let Some(card) = feed
        .iter()
        .find(|c| c.card_type == "head2head" && c.head2head.variant != "belt")
    else {
        return Head2headSnap {
            present: false,
            placement: "none",
            member_count: 0,
            open_seats: 0,
            own_values: Vec::new(),
            titles: Vec::new(),
            pairs_count: 0,
            pair_rows: 0,
            pair_rows_dot: 0,
            more_pairs: 0,
            belt_moved_recent: false,
            own_avatar_key: String::new(),
            pair_avatar_keys: Vec::new(),
        };
    };
    let h = &card.head2head;
    let members = app.get_members();
    Head2headSnap {
        present: true,
        placement: placement_name(&h.variant),
        member_count: h.member_count,
        open_seats: h.open_seats,
        own_values: first_rows(&h.chips, |c| TitleChipSnap {
            title_id: c.id.to_string(),
            display: if c.unit.is_empty() {
                c.value.to_string()
            } else {
                format!("{} {}", c.value, c.unit)
            },
        }),
        titles: first_rows(&h.titles, |t| TitleHolderSnap {
            title_id: t.id.to_string(),
            holder_name: members
                .iter()
                .find(|m| m.id == t.holder_id)
                .map(|m| m.name.to_string())
                .unwrap_or_default(),
        }),
        pairs_count: h.pairs_total.max(0) as usize,
        pair_rows: h.pairs.row_count(),
        pair_rows_dot: h.pairs.iter().filter(|r| r.moved_recent).count(),
        more_pairs: h.more_pairs,
        belt_moved_recent: h.belt_moved_recent,
        own_avatar_key: if h.left_has_avatar {
            image_key(&h.left_avatar)
        } else {
            String::new()
        },
        pair_avatar_keys: h
            .pairs
            .iter()
            .map(|r| {
                if r.has_avatar {
                    image_key(&r.avatar)
                } else {
                    String::new()
                }
            })
            .collect(),
    }
}

/// The invite card of the feed, read from the first `invite` card.
fn read_invite_card(app: &MainWindow) -> InviteCardSnap {
    let feed = app.get_feed_cards();
    match feed.iter().find(|c| c.card_type == "invite") {
        Some(card) => InviteCardSnap {
            present: true,
            octagons: card.head2head.members.row_count(),
            more_count: card.head2head.more_members,
            with_avatar: card
                .head2head
                .members
                .iter()
                .filter(|m| m.has_avatar)
                .count(),
        },
        None => InviteCardSnap {
            present: false,
            octagons: 0,
            more_count: 0,
            with_avatar: 0,
        },
    }
}

/// The daily recap card of the feed, read from the first such card.
fn read_daily_recap(app: &MainWindow) -> DailyRecapSnap {
    let feed = app.get_feed_cards();
    let Some((index, card)) = feed
        .iter()
        .enumerate()
        .find(|(_, c)| c.card_type == "daily-recap")
    else {
        return DailyRecapSnap {
            present: false,
            day: String::new(),
            hangout: String::new(),
            clips: 0,
            sessions: 0,
            top_game: String::new(),
            members: 0,
            index: 0,
            belts: Vec::new(),
        };
    };
    DailyRecapSnap {
        present: true,
        day: card.day_label.to_string(),
        hangout: card.title.to_string(),
        clips: card.clip_count,
        sessions: card.day_sessions,
        top_game: card.game_name.to_string(),
        members: card.day_members.row_count(),
        index,
        belts: card
            .belts_moved
            .iter()
            .map(|b| b.title.to_string())
            .collect(),
    }
}

/// The recent games card of the feed, read from the first such card.
fn read_recent_games(app: &MainWindow) -> RecentGamesSnap {
    let feed = app.get_feed_cards();
    let Some(card) = feed.iter().find(|c| c.card_type == "recent-games") else {
        return RecentGamesSnap {
            header_line: String::new(),
            shape: "",
            pc_count: 0,
            tiles: Vec::new(),
        };
    };
    RecentGamesSnap {
        header_line: card.subtitle.to_string(),
        shape: if crate::recent_games::is_grid(i64::from(card.recent_pc_count)) {
            "grid"
        } else {
            "list"
        },
        pc_count: card.recent_pc_count,
        tiles: first_rows(&card.recent_games, |t| {
            // The tile shows the launcher in place of hours when it has none.
            let hours = t.hours.to_string();
            let launcher = if hours.starts_with(|c: char| c.is_ascii_digit()) {
                crate::recent_games::launcher_of(&t.game_id, "")
            } else {
                hours.to_lowercase()
            };
            RecentGameTileSnap {
                name: t.game_name.to_string(),
                owners: t.owners.row_count(),
                hours_text: hours,
                launcher,
                tracked: t.tracked,
                art: if t.has_capsule || t.has_icon {
                    "image"
                } else {
                    "monogram"
                },
            }
        }),
    }
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
        transport,
        connected,
        reconnecting,
    }
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
        feed_cards: first_rows(&app.get_feed_cards(), |c| FeedCardSnap {
            id: c.id.to_string(),
            card_type: c.card_type.to_string(),
            title: c.title.to_string(),
            actor_name: c.actor_name.to_string(),
            clip_path: c.clip_path.to_string(),
        }),
        clip: ClipSnap {
            path: app.get_clip_playing_path().into(),
            paused: app.get_clip_paused(),
            progress: app.get_clip_progress(),
            duration_ms: app.get_clip_duration_ms(),
        },
        head2head: read_head2head(app),
        sealed: read_sealed(app),
        invite_card: read_invite_card(app),
        recent_games: read_recent_games(app),
        daily_recap: read_daily_recap(app),
        live_sessions: app
            .get_feed_cards()
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
        joined: app
            .get_feed_cards()
            .iter()
            .enumerate()
            .filter(|(_, c)| c.card_type == "joined")
            .map(|(index, c)| JoinedSnap {
                name: c.actor_name.to_string(),
                title: c.title.to_string(),
                invited: c.subtitle.to_string(),
                when: c.timestamp.to_string(),
                index,
            })
            .collect(),
        chat_input_focused: app.global::<crate::ChatFocus>().get_input_focused(),
        quiet_rows: app
            .get_feed_quiet()
            .iter()
            .map(|r| JoinedRowSnap {
                line: r.line.to_string(),
                when: r.when.to_string(),
            })
            .collect(),
        dormant_strip: app.get_feed_dormant().iter().next().map(|c| DormantSnap {
            text: c.title.to_string(),
            age: c.timestamp.to_string(),
        }),
        belt_hero: app
            .get_feed_cards()
            .iter()
            .find(|c| c.card_type == "head2head" && c.head2head.variant == "belt")
            .map(|c| BeltHeroSnap {
                headline: c.head2head.belt.headline.to_string(),
                detail: c.head2head.belt.detail.to_string(),
                age: c.head2head.belt.age.to_string(),
            }),
        skeleton_trio: [
            app.get_feed_skeleton_stream(),
            app.get_feed_skeleton_voice_clip(),
            app.get_feed_skeleton_recap(),
        ]
        .into_iter()
        .filter(|on| *on)
        .count(),
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
        app.set_feed_cards(model(vec![crate::FeedCardData {
            id: "f1".into(),
            card_type: "clip".into(),
            title: "Clutch".into(),
            actor_name: "alice".into(),
            clip_path: "/clips/a.mp4".into(),
            ..Default::default()
        }]));
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
                    "edited": true, "deleted": false, "gif": true, "mentions_me": true}])
        );
        assert_eq!(
            s["stream"],
            json!({"hosting": true, "watching": false, "width": 1280, "height": 720,
                   "viewer_count": 1, "preset": 3, "frames_presented": 42,
                   "cards": [{"host_id": "u1", "streamer_name": "alice", "viewer_count": 1,
                              "width": 1280, "height": 720, "can_watch": true}]})
        );
        assert_eq!(
            s["feed_cards"],
            json!([{"id": "f1", "card_type": "clip", "title": "Clutch",
                    "actor_name": "alice", "clip_path": "/clips/a.mp4"}])
        );
        assert_eq!(
            s["clip"],
            json!({"path": "/clips/a.mp4", "paused": true, "progress": 0.5, "duration_ms": 30000})
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
        let chip = |key: &str, sealed: bool| crate::TitleHeld {
            belt: belt(key, sealed),
            ..Default::default()
        };
        let row = |key: &str, sealed: bool| crate::H2hPairRow {
            has_chip: true,
            chip: chip(key, sealed),
            ..Default::default()
        };
        app.set_feed_cards(model(vec![
            crate::FeedCardData {
                card_type: "head2head".into(),
                head2head: crate::Head2headData {
                    pairs: model(vec![row("night_owl:1", true), row("the_carry:2", false)]),
                    ..Default::default()
                },
                ..Default::default()
            },
            crate::FeedCardData {
                card_type: "recap".into(),
                recap_seal: crate::RecapSeal {
                    key: "recap:7".into(),
                    sealed: true,
                    ..Default::default()
                },
                // The same belt as the pair row: counted once.
                belts_moved: model(vec![crate::RecapBeltMovedRow {
                    belt: belt("night_owl:1", true),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        ]));
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
    fn the_snapshot_reads_head2head_recent_games_and_the_skeleton_trio() {
        let h = Harness::new();
        let app = h.app();
        let none = snapshot_json(app);
        assert_eq!(none["head2head"]["present"], false);
        assert_eq!(none["head2head"]["placement"], "none");
        assert_eq!(
            none["recent_games"],
            json!({"header_line": "", "shape": "", "pc_count": 0, "tiles": []})
        );
        assert_eq!(none["skeleton_trio"], 0);

        app.set_members(model(vec![crate::MemberData {
            id: "u1".into(),
            name: "bob".into(),
            ..Default::default()
        }]));
        let chip = |id: &str, value: &str, unit: &str| crate::TitleHeld {
            id: id.into(),
            value: value.into(),
            unit: unit.into(),
            holder_id: "u1".into(),
            ..Default::default()
        };
        let tile = |name: &str, id: &str, hours: &str, capsule: bool| crate::RecentGameTile {
            game_id: id.into(),
            game_name: name.into(),
            hours: hours.into(),
            has_capsule: capsule,
            tracked: capsule,
            ..Default::default()
        };
        app.set_feed_cards(model(vec![
            crate::FeedCardData {
                card_type: "head2head".into(),
                head2head: crate::Head2headData {
                    variant: "cell-full".into(),
                    member_count: 1,
                    open_seats: 4,
                    belt_moved_recent: true,
                    pairs_total: 4,
                    more_pairs: 3,
                    left_has_avatar: true,
                    left_avatar: pixels([9, 9, 9, 255]),
                    pairs: model(vec![crate::H2hPairRow::default()]),
                    chips: model(vec![
                        chip("the_carry", "62", "%"),
                        chip("one_trick", "9", ""),
                    ]),
                    titles: model(vec![chip("the_carry", "62", "%")]),
                    ..Default::default()
                },
                ..Default::default()
            },
            crate::FeedCardData {
                card_type: "recent-games".into(),
                subtitle: "14 sessions, 31 h this week".into(),
                recent_games: model(vec![
                    tile("Brawlhalla", "steam-291550", "4,167 H", true),
                    tile("Fortnite", "epic-fortnite", "EPIC", false),
                ]),
                ..Default::default()
            },
        ]));
        app.set_feed_skeleton_stream(true);
        app.set_feed_skeleton_recap(true);

        let s = snapshot_json(app);
        assert_eq!(
            s["head2head"],
            json!({"present": true, "placement": "cell", "member_count": 1, "open_seats": 4,
                   "own_values": [{"title_id": "the_carry", "display": "62 %"},
                                  {"title_id": "one_trick", "display": "9"}],
                   "titles": [{"title_id": "the_carry", "holder_name": "bob"}],
                   "pairs_count": 4, "pair_rows": 1, "pair_rows_dot": 0, "more_pairs": 3, "belt_moved_recent": true,
                   "own_avatar_key": image_key(&pixels([9, 9, 9, 255])),
                   "pair_avatar_keys": [""]})
        );
        assert_eq!(
            s["invite_card"],
            json!({"present": false, "octagons": 0, "more_count": 0, "with_avatar": 0})
        );
        assert_eq!(
            s["recent_games"],
            json!({"header_line": "14 sessions, 31 h this week", "shape": "list", "pc_count": 0, "tiles": [
                {"name": "Brawlhalla", "hours_text": "4,167 H", "launcher": "steam",
                 "owners": 0, "tracked": true, "art": "image"},
                {"name": "Fortnite", "hours_text": "EPIC", "launcher": "epic",
                 "owners": 0, "tracked": false, "art": "monogram"}]})
        );
        assert_eq!(s["skeleton_trio"], 2);
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
