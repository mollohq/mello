//! Test-only state port for the e2e driver (`e2e` feature, never shipped).
//!
//! Serves the app's state as JSON on `127.0.0.1:$MELLO_E2E_STATE_PORT`, so a
//! driver can wait on real state instead of sleeping or reading pixels
//! (see plans/E2E-QA.md §5.1). Phase 0 prototype: plain `std::net`, no new
//! dependencies, GET only.
//!
//! - `GET /state`  — a snapshot read on the UI thread.
//! - `GET /events` — the last [`EVENT_TAIL`] core events, oldest first.
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
    /// Only for `VoiceStateChanged`: `"sfu"`, `"p2p"` or `"disconnected"`.
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
    /// The transport of the current voice call: `"sfu"` or `"p2p"`. `None`
    /// (JSON `null`) when not in a call. See [`transport_after`].
    voice_transport: Option<&'static str>,
    /// The other members of the watched SFU stream: the host and the other
    /// viewers, in join order. Empty when not watching, and in P2P mode.
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
    name: String,
    speaking: bool,
    muted: bool,
    deafened: bool,
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
        VoiceMode::P2P => "p2p",
        VoiceMode::Disconnected => "disconnected",
    }
}

/// The transport of the current voice call after `ev`, from `prev`.
///
/// The core reports the transport in `VoiceStateChanged` when a call starts
/// on the SFU or on P2P (an SFU join that failed and fell back), and when the
/// call ends. `VoiceSfuDisconnected` means the SFU call dropped: there is no
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
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let (status, body) = match path {
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
    rx.recv_timeout(Duration::from_secs(2)).ok()
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
                        name: m.name.to_string(),
                        speaking: m.speaking,
                        muted: m.muted,
                        deafened: m.deafened,
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
            transport_after(None, &state_changed(true, VoiceMode::P2P)),
            Some("p2p"),
            "an SFU join that fell back to P2P must show as p2p"
        );
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
