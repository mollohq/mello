//! Test-only state port for the e2e driver (`e2e` feature, never shipped).
//!
//! Serves the app's state as JSON on `127.0.0.1:$MELLO_E2E_STATE_PORT`, so a
//! driver can wait on real state instead of sleeping or reading pixels
//! (see plans/E2E-QA.md §5.1). Phase 0 prototype: plain `std::net`, no new
//! dependencies, GET only.
//!
//! - `GET /state`  — a snapshot read on the UI thread.
//! - `GET /events` — the last [`EVENT_TAIL`] core events, oldest first.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mello_core::Event;
use serde::Serialize;
use slint::{ComponentHandle, Model};

use crate::MainWindow;

pub const PORT_ENV: &str = "MELLO_E2E_STATE_PORT";
const EVENT_TAIL: usize = 200;

static EVENTS: Mutex<VecDeque<EventRecord>> = Mutex::new(VecDeque::new());
static EVENT_SEQ: Mutex<u64> = Mutex::new(0);

#[derive(Serialize, Clone)]
struct EventRecord {
    seq: u64,
    ts_ms: u128,
    #[serde(rename = "type")]
    kind: String,
    /// Only for `Error`: the UI never shows these, so the driver must see them.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
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
    /// Modals that are open, by name. The driver waits on these to close.
    open_modals: Vec<&'static str>,
    /// The last 20 chat messages in the active crew, oldest first.
    messages: Vec<MessageSnap>,
    voice_channels: Vec<VoiceChannelSnap>,
    last_event_seq: u64,
}

#[derive(Serialize)]
struct MessageSnap {
    sender: String,
    text: String,
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

/// Record a core event before the UI handles it. Called from the poll loop.
pub fn record_event(ev: &Event) {
    let message = match ev {
        Event::Error { message } => Some(message.clone()),
        _ => None,
    };
    let seq = {
        let mut s = EVENT_SEQ.lock().expect("event seq lock");
        *s += 1;
        *s
    };
    let rec = EventRecord {
        seq,
        ts_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default(),
        kind: perf_scenarios::event_type(ev).to_string(),
        message,
    };
    let mut q = EVENTS.lock().expect("event tail lock");
    if q.len() == EVENT_TAIL {
        q.pop_front();
    }
    q.push_back(rec);
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
    let (tx, rx) = mpsc::channel();
    let weak = weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            let _ = tx.send(serde_json::to_string(&read(&app)).unwrap_or_default());
        }
    })
    .ok()?;
    rx.recv_timeout(Duration::from_secs(2)).ok()
}

fn read(app: &MainWindow) -> Snapshot {
    let step = app.get_onboarding_step();
    let logged_in = app.get_logged_in();
    let show_sign_in = app.get_show_sign_in();
    // Mirrors the branch conditions in main.slint.
    let screen = if show_sign_in {
        "sign_in"
    } else if (1..=3).contains(&step) {
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
        active_crew_id: app.get_active_crew_id().into(),
        active_crew_name: app.get_active_crew_name().into(),
        crews: crews.iter().map(|c| c.name.to_string()).collect(),
        members: members.iter().map(|m| m.name.to_string()).collect(),
        in_voice: app.get_in_voice(),
        join_crew_modal_open: app.get_join_crew_modal_open(),
        join_crew_name: app.get_join_crew_name().into(),
        join_crew_error: app.get_join_crew_error().into(),
        mic_muted: app.get_mic_muted(),
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
                })
                .collect();
            let skip = all.len().saturating_sub(20);
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
        last_event_seq: *EVENT_SEQ.lock().expect("event seq lock"),
    }
}
