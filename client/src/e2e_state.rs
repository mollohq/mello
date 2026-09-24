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
    active_crew_id: String,
    active_crew_name: String,
    crews: Vec<String>,
    members: Vec<String>,
    in_voice: bool,
    join_crew_modal_open: bool,
    join_crew_name: String,
    join_crew_error: String,
    last_event_seq: u64,
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
        active_crew_id: app.get_active_crew_id().into(),
        active_crew_name: app.get_active_crew_name().into(),
        crews: crews.iter().map(|c| c.name.to_string()).collect(),
        members: members.iter().map(|m| m.name.to_string()).collect(),
        in_voice: app.get_in_voice(),
        join_crew_modal_open: app.get_join_crew_modal_open(),
        join_crew_name: app.get_join_crew_name().into(),
        join_crew_error: app.get_join_crew_error().into(),
        last_event_seq: *EVENT_SEQ.lock().expect("event seq lock"),
    }
}
