//! Stream start, rejoin and the Nakama stall against a fake SFU with live
//! native peers (streaming-quality stage 6).
//!
//! The fake SFU answers each stream offer with a native libmello peer of the
//! other role and relays ICE candidates both ways. Like mello-sfu, it
//! answers the client's WebSocket pings (the WebSocket library does) and no
//! control-channel ping of a stream peer. A test makes it silent (the socket
//! is not read: no pong, as on a pulled cable) or drops its socket (a
//! closed connection). The client runs its real command loop with a libmello
//! context on the device-free audio backend.

use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::Message;

use super::stream_host_start::{HostLinkState, HostMsg};
use super::stream_link::StreamLink;
use super::streaming::{join_stream_session, StreamGrant, StreamJoinLimits};
use super::Client;
use crate::command::Command;
use crate::config::Config;
use crate::events::{Event, StreamRole};
use crate::stream::manager::StreamSession;
use crate::stream::sink_sfu::SfuSink;
use crate::transport::sfu_connection::{SfuEvent, StreamPeerRole};
use crate::voice::VoiceManager;

const LIMIT: Duration = Duration::from_secs(20);
const CREW: &str = "crew-streamed";
const SESSION: &str = "stream_host_1";

// ---------------------------------------------------------------------------
// A fake Nakama HTTP API
// ---------------------------------------------------------------------------

type Rpcs = Arc<Mutex<Vec<(String, serde_json::Value, Instant)>>>;

/// A fake Nakama HTTP API. RPCs named in `fail` answer an error; RPCs named
/// in `stall` answer after `stall_for`; every call is recorded when it
/// arrives.
async fn fake_nakama(
    fail: &'static [&'static str],
    stall: &'static [&'static str],
    stall_for: Duration,
) -> (u16, Rpcs) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let port = listener.local_addr().expect("listener address").port();
    let rpcs: Rpcs = Arc::default();
    let seen = Arc::clone(&rpcs);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or_default();
                let name = path
                    .strip_prefix("/v2/rpc/")
                    .unwrap_or(path)
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let payload = serde_json::from_slice::<String>(&body)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or(serde_json::Value::Null);
                let failed = fail.contains(&name.as_str());
                let stalled = stall.contains(&name.as_str());
                seen.lock()
                    .expect("rpc log")
                    .push((name, payload, Instant::now()));
                if stalled {
                    tokio::time::sleep(stall_for).await;
                }
                let (status, reply) = if failed {
                    ("404 Not Found", r#"{"error":"not found","code":5}"#)
                } else {
                    ("200 OK", r#"{"payload":"{}"}"#)
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = write.write_all(response.as_bytes()).await;
            });
        }
    });
    (port, rpcs)
}

fn rpc_calls(rpcs: &Rpcs, name: &str) -> Vec<serde_json::Value> {
    rpcs.lock()
        .expect("rpc log")
        .iter()
        .filter(|(n, _, _)| n == name)
        .map(|(_, p, _)| p.clone())
        .collect()
}

fn client_for(port: u16) -> (Client, mpsc::Receiver<Event>) {
    let mut config = Config::development();
    config.nakama_host = "127.0.0.1".into();
    config.nakama_port = port;
    config.nakama_ssl = false;
    let (event_tx, events) = mpsc::channel();
    let voice = VoiceManager::with_test_audio_backend(event_tx.clone());
    let mut client = Client::with_voice(
        config,
        event_tx,
        voice,
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(None)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU8::new(0)),
        false,
        false,
    );
    client.nakama.set_test_session("session-token", None);
    (client, events)
}

/// Wait for the first event that `pick` accepts; returns it and the events
/// before it. Polls, because the loop runs on the test's thread.
async fn wait_for_event(
    events: &mpsc::Receiver<Event>,
    what: &str,
    limit: Duration,
    pick: impl Fn(&Event) -> bool,
) -> (Event, Vec<Event>) {
    let deadline = tokio::time::Instant::now() + limit;
    let mut before = Vec::new();
    loop {
        while let Ok(ev) = events.try_recv() {
            if pick(&ev) {
                return (ev, before);
            }
            before.push(ev);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no {what} event within {limit:?}; saw {before:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// A fake stream SFU with live native peers
// ---------------------------------------------------------------------------

unsafe extern "C" fn on_fake_candidate(
    user_data: *mut std::ffi::c_void,
    candidate: *const mello_sys::MelloIceCandidate,
) {
    if user_data.is_null() || candidate.is_null() {
        return;
    }
    let queue = &*(user_data as *const Mutex<Vec<(String, String, i32)>>);
    let c = &*candidate;
    let cand = CStr::from_ptr(c.candidate).to_string_lossy().into_owned();
    let mid = CStr::from_ptr(c.sdp_mid).to_string_lossy().into_owned();
    if let Ok(mut q) = queue.lock() {
        q.push((cand, mid, c.sdp_mline_index));
    }
}

fn text(value: serde_json::Value) -> Message {
    Message::Text(value.to_string())
}

/// What the fake SFU saw, and the controls of a test.
#[derive(Clone)]
struct MediaStreamSfu {
    endpoint: String,
    /// `(session_id, role, when)` of each `join_stream`.
    joins: Arc<Mutex<Vec<(String, String, Instant)>>>,
    /// Connections with an id below this are silent (a pulled cable): their
    /// socket is not read, so no pong. A later connection (the cable back)
    /// answers.
    silent_below: Arc<std::sync::atomic::AtomicUsize>,
    next_connection: Arc<std::sync::atomic::AtomicUsize>,
    /// Close the socket of every connection now, with no close frame.
    drop_all: Arc<tokio::sync::Notify>,
    /// `leave` messages received.
    leaves: Arc<Mutex<Vec<Instant>>>,
}

impl MediaStreamSfu {
    /// Pull the cable of every connection open now.
    fn silence(&self) {
        let open = self.next_connection.load(Ordering::Acquire);
        self.silent_below.store(open, Ordering::Release);
    }

    /// Plug it back in.
    fn unsilence(&self) {
        self.silent_below.store(0, Ordering::Release);
    }
}

/// A fake stream SFU that takes any number of connections.
async fn media_stream_sfu() -> MediaStreamSfu {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let sfu = MediaStreamSfu {
        endpoint: format!(
            "ws://{}/ws",
            listener.local_addr().expect("listener has an address")
        ),
        joins: Arc::default(),
        silent_below: Arc::default(),
        next_connection: Arc::default(),
        drop_all: Arc::new(tokio::sync::Notify::new()),
        leaves: Arc::default(),
    };
    let shared = sfu.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let shared = shared.clone();
            tokio::spawn(async move { serve_stream_connection(socket, shared).await });
        }
    });
    sfu
}

async fn serve_stream_connection(socket: tokio::net::TcpStream, sfu: MediaStreamSfu) {
    let id = sfu.next_connection.fetch_add(1, Ordering::AcqRel);
    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
        return;
    };
    if ws
        .send(text(serde_json::json!({
            "type": "welcome",
            "data": { "server_id": "fake", "region": "test" }
        })))
        .await
        .is_err()
    {
        return;
    }
    let candidates: Arc<Mutex<Vec<(String, String, i32)>>> = Arc::default();
    let mut peer: usize = 0;
    let mut early = Vec::new();
    let mut relay = tokio::time::interval(Duration::from_millis(10));
    let mut dropped_by_test = false;
    let dropped = sfu.drop_all.notified();
    tokio::pin!(dropped);
    loop {
        tokio::select! {
            _ = &mut dropped => {
                dropped_by_test = true;
                break;
            }
            // While silent the socket is not read: no WebSocket pong, as on
            // a pulled cable. Like mello-sfu, the fake answers no
            // control-channel ping of a stream peer.
            msg = ws.next(), if id >= sfu.silent_below.load(Ordering::Acquire) => {
                let v: serde_json::Value = match msg {
                    Some(Ok(Message::Text(t))) => match serde_json::from_str(&t) {
                        Ok(v) => v,
                        Err(_) => continue,
                    },
                    Some(Ok(_)) => continue,
                    _ => break,
                };
                match v["type"].as_str() {
                    Some("join_stream") => {
                        let session = v["data"]["session_id"].as_str().unwrap_or("").to_string();
                        let role = v["data"]["role"].as_str().unwrap_or("").to_string();
                        sfu.joins.lock().expect("joins").push((session.clone(), role, Instant::now()));
                        let _ = ws.send(text(serde_json::json!({
                            "type": "joined",
                            "data": { "session_type": "stream", "session_id": session, "members": [] }
                        }))).await;
                    }
                    Some("offer") => {
                        let offer = v["data"]["sdp"].as_str().unwrap_or("").to_string();
                        // The other role answers: a host's offer gets a viewer peer.
                        let host_offer = sfu.joins.lock().expect("joins").last().is_some_and(|j| j.1 == "host");
                        let role = if host_offer {
                            mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
                        } else {
                            mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_HOST
                        };
                        let id = CString::new("fake-sfu").expect("static id");
                        let offer_c = CString::new(offer).expect("offer has no NUL");
                        // SAFETY: a null context is allowed; the callback
                        // state outlives the peer (destroyed below first).
                        let answer = unsafe {
                            let p = mello_sys::mello_peer_create_for_role(std::ptr::null_mut(), id.as_ptr(), role);
                            assert!(!p.is_null(), "fake SFU peer");
                            mello_sys::mello_peer_set_ice_callback(
                                p,
                                Some(on_fake_candidate),
                                Arc::as_ptr(&candidates) as *mut std::ffi::c_void,
                            );
                            let answer = mello_sys::mello_peer_create_answer(p, offer_c.as_ptr());
                            assert!(!answer.is_null(), "fake SFU answer");
                            peer = p as usize;
                            CStr::from_ptr(answer).to_string_lossy().into_owned()
                        };
                        for msg in early.drain(..) {
                            add_candidate(peer, &msg);
                        }
                        let _ = ws.send(text(serde_json::json!({ "type": "answer", "data": { "sdp": answer } }))).await;
                    }
                    Some("ice_candidate") => {
                        if peer == 0 {
                            early.push(v);
                        } else {
                            add_candidate(peer, &v);
                        }
                    }
                    Some("leave") => {
                        sfu.leaves.lock().expect("leaves").push(Instant::now());
                        let _ = ws.send(text(serde_json::json!({ "type": "left", "data": {} }))).await;
                    }
                    _ => {}
                }
            }
            _ = relay.tick() => {
                let pending: Vec<_> = candidates.lock().expect("candidates").drain(..).collect();
                for (candidate, mid, idx) in pending {
                    let _ = ws.send(text(serde_json::json!({
                        "type": "ice_candidate",
                        "data": { "candidate": candidate, "sdp_mid": mid, "sdp_mline_index": idx }
                    }))).await;
                }
            }
        }
    }
    // The socket drops here with no close frame: an unclean end.
    drop(ws);
    if dropped_by_test {
        // Only the signaling socket went: the native peer keeps its media
        // and its pongs for a while, so the client sees the drop through
        // the socket alone.
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
    if peer != 0 {
        // SAFETY: the peer is destroyed once; its callbacks end first.
        unsafe {
            let p = peer as *mut mello_sys::MelloPeerConnection;
            mello_sys::mello_peer_set_ice_callback(p, None, std::ptr::null_mut());
            mello_sys::mello_peer_destroy(p);
        }
    }
}

fn add_candidate(peer: usize, msg: &serde_json::Value) {
    let cand = msg["data"]["candidate"].as_str().unwrap_or("");
    let mid = msg["data"]["sdp_mid"].as_str().unwrap_or("0");
    let cand = CString::new(cand).expect("candidate");
    let mid = CString::new(mid).expect("mid");
    let c = mello_sys::MelloIceCandidate {
        candidate: cand.as_ptr(),
        sdp_mid: mid.as_ptr(),
        sdp_mline_index: msg["data"]["sdp_mline_index"].as_i64().unwrap_or(0) as i32,
    };
    // SAFETY: the peer is live; the strings outlive the call.
    unsafe {
        mello_sys::mello_peer_add_ice_candidate(peer as *mut mello_sys::MelloPeerConnection, &c)
    };
}

fn limits() -> StreamJoinLimits {
    StreamJoinLimits {
        connect: Duration::from_secs(5),
        session: Duration::from_secs(10),
    }
}

/// A host joined to the fake SFU with its track open, as the start leaves
/// it: the session, the sink on the connection, the link.
async fn live_host(client: &mut Client, sfu: &MediaStreamSfu) -> Arc<SfuSink> {
    let (conn, _) = join_stream_session(
        sfu.endpoint.clone(),
        "token".into(),
        SESSION.into(),
        StreamPeerRole::Host,
        0,
        Vec::new(),
        limits(),
    )
    .await
    .expect("the host joins the fake SFU with its track open");
    let conn = Arc::new(conn);
    let sink = Arc::new(SfuSink::new(Arc::clone(&conn)));
    let (stop_tx, _stop_rx) = tokio::sync::oneshot::channel();
    client.stream_session = Some(
        StreamSession::new(
            SESSION.to_string(),
            "sfu".to_string(),
            stop_tx,
            tokio::spawn(async {}),
        )
        .with_crew_id(CREW),
    );
    let sink_dyn: Arc<dyn crate::stream::sink::PacketSink> = sink.clone();
    client.stream_host_sink = Some(sink_dyn);
    client.stream_host_sfu_sink = Some(Arc::clone(&sink));
    client.stream_sfu_connection = Some(conn);
    client.host_link = Some(HostLinkState {
        generation: 1,
        link: StreamLink::new(StreamRole::Host),
        grant: StreamGrant::new(sfu.endpoint.clone(), "token".into()),
        session_id: SESSION.into(),
        connected_at: Instant::now(),
        attempt: None,
        token_task: None,
        token_tried_at: None,
        ticks: 0,
        start_timing: None,
    });
    sink
}

fn is_reconnecting_host(ev: &Event) -> bool {
    matches!(
        ev,
        Event::StreamReconnecting {
            role: StreamRole::Host
        }
    )
}

fn is_resumed_host(ev: &Event) -> bool {
    matches!(
        ev,
        Event::StreamResumed {
            role: StreamRole::Host
        }
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The start runs off the loop and asks the backend to advertise the stream
/// when its track is open. A failed `start_stream` ends the start with a
/// cause for the user and starts no capture. Before stage 6 the start
/// awaited the RPC on the loop, the backend advertised at once, and a
/// failure was a log-only `StreamError`.
#[tokio::test]
async fn a_start_asks_for_the_advert_on_live_and_a_failed_rpc_names_the_cause() {
    let (port, rpcs) = fake_nakama(&["start_stream"], &[], Duration::ZERO).await;
    let (mut client, events) = client_for(port);

    tokio::time::timeout(
        Duration::from_millis(500),
        client.handle_start_stream(CREW, "Raid", "monitor", Some(0), None, None, 2, "", 0),
    )
    .await
    .expect("the StartStream handler returns at once");
    assert!(client.host_start.is_some());
    assert!(matches!(
        events.try_recv(),
        Ok(Event::StreamStarting { ref crew_id }) if crew_id == CREW
    ));

    let msg = tokio::time::timeout(LIMIT, client.stream_tasks.next())
        .await
        .expect("the RPC step reports");
    client.on_stream_task(msg).await;

    let payload = rpc_calls(&rpcs, "start_stream")
        .pop()
        .expect("start_stream sent");
    assert_eq!(payload["advertise"], "on_live", "{payload}");
    assert_eq!(payload["crew_id"], CREW);
    assert!(payload["fps"].as_u64().is_some_and(|f| f > 0), "{payload}");
    assert!(client.host_start.is_none());
    let (failed, _) = wait_for_event(&events, "StreamFailed", LIMIT, |e| {
        matches!(e, Event::StreamFailed { .. })
    })
    .await;
    assert!(
        matches!(
            failed,
            Event::StreamFailed { role: StreamRole::Host, ref reason }
                if reason == "The server did not accept the stream."
        ),
        "{failed:?}"
    );
}

/// The stream goes live when both the capture and the SFU track are up:
/// then the UI gets `StreamStarted` and the backend `stream_live` with the
/// encode size. The sink asks for an IDR on the open track.
#[tokio::test]
async fn the_stream_goes_live_when_the_capture_and_the_track_are_up() {
    let (port, rpcs) = fake_nakama(&[], &[], Duration::ZERO).await;
    let (mut client, events) = client_for(port);
    let sfu = media_stream_sfu().await;
    let generation =
        super::streaming::stream_lifecycle_tests::install_host_start(&mut client, &sfu.endpoint);

    client
        .on_host_msg(HostMsg::StartNative {
            generation,
            result: Ok(super::streaming::stream_lifecycle_tests::native_outcome()),
        })
        .await;
    assert!(
        client.stream_session.is_none(),
        "not live before the track opens"
    );
    assert!(!events
        .try_iter()
        .any(|e| matches!(e, Event::StreamStarted { .. })));
    assert!(rpc_calls(&rpcs, "stream_live").is_empty());

    let result = join_stream_session(
        sfu.endpoint.clone(),
        "token".into(),
        SESSION.into(),
        StreamPeerRole::Host,
        0,
        Vec::new(),
        limits(),
    )
    .await
    .map(|(conn, _)| conn);
    client
        .on_host_msg(HostMsg::StartSfu { generation, result })
        .await;

    assert!(client.stream_session.is_some() && client.host_link.is_some());
    let (started, _) = wait_for_event(&events, "StreamStarted", LIMIT, |e| {
        matches!(e, Event::StreamStarted { .. })
    })
    .await;
    assert!(matches!(started, Event::StreamStarted { ref crew_id, .. } if crew_id == CREW));
    let deadline = tokio::time::Instant::now() + LIMIT;
    while rpc_calls(&rpcs, "stream_live").is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no stream_live");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let live = rpc_calls(&rpcs, "stream_live").remove(0);
    assert_eq!(live["crew_id"], CREW);
    assert_eq!(live["session_id"], SESSION);
    assert_eq!(
        (live["width"].as_u64(), live["height"].as_u64()),
        (Some(1280), Some(720))
    );
    // The manager runs on the sink, which now sends on the open track. The
    // manager takes the attach's IDR request itself; the rejoin test sees it.
    let sink = client.stream_host_sfu_sink.clone().expect("the live sink");
    assert!(sink.connection().is_some_and(|c| c.is_video_track_open()));
}

/// A link that goes silent (a pulled cable) shows "reconnecting" within
/// 1 s; when the pongs come back within 5 s the stream resumes on the same
/// connection, with no rejoin.
#[tokio::test]
async fn a_short_silence_shows_reconnecting_and_resumes_on_the_same_connection() {
    let (port, _rpcs) = fake_nakama(&[], &[], Duration::ZERO).await;
    let (mut client, events) = client_for(port);
    let sfu = media_stream_sfu().await;
    live_host(&mut client, &sfu).await;
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    let driver = async {
        // Pongs flow first: the link is live.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let silent_at = Instant::now();
        sfu.silence();
        wait_for_event(
            &events,
            "StreamReconnecting",
            Duration::from_secs(3),
            is_reconnecting_host,
        )
        .await;
        let shown_after = silent_at.elapsed();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        sfu.unsilence();
        wait_for_event(
            &events,
            "StreamResumed",
            Duration::from_secs(3),
            is_resumed_host,
        )
        .await;
        drop(cmd_tx);
        shown_after
    };
    let (_, shown_after) =
        tokio::time::timeout(LIMIT, async { tokio::join!(client.run(cmd_rx), driver) })
            .await
            .expect("the loop ends when the command channel closes");
    assert!(
        shown_after <= Duration::from_secs(1),
        "reconnecting shown {shown_after:?} after the link went silent"
    );
    assert_eq!(
        sfu.joins.lock().expect("joins").len(),
        1,
        "a short silence is no rejoin"
    );
}

/// A dropped SFU connection rejoins the same session as the host on the
/// voice schedule, with no click: the sink sends on the new connection and
/// asks for an IDR. Before stage 6 the drop was logged and the encoder sent
/// into a dead peer.
#[tokio::test]
async fn a_dropped_host_connection_rejoins_the_same_session() {
    let (port, _rpcs) = fake_nakama(&[], &[], Duration::ZERO).await;
    let (mut client, events) = client_for(port);
    let sfu = media_stream_sfu().await;
    let sink = live_host(&mut client, &sfu).await;
    let first = sink.connection().expect("the first connection");
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    let driver = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let dropped_at = Instant::now();
        sfu.drop_all.notify_waiters();
        wait_for_event(
            &events,
            "StreamReconnecting",
            Duration::from_secs(3),
            is_reconnecting_host,
        )
        .await;
        wait_for_event(
            &events,
            "StreamResumed",
            Duration::from_secs(8),
            is_resumed_host,
        )
        .await;
        let resumed_after = dropped_at.elapsed();
        drop(cmd_tx);
        resumed_after
    };
    let (_, resumed_after) =
        tokio::time::timeout(LIMIT, async { tokio::join!(client.run(cmd_rx), driver) })
            .await
            .expect("the loop ends when the command channel closes");

    let joins = sfu.joins.lock().expect("joins").clone();
    assert_eq!(joins.len(), 2, "one rejoin: {joins:?}");
    assert!(
        joins.iter().all(|(s, r, _)| s == SESSION && r == "host"),
        "the same session, as the host: {joins:?}"
    );
    let second = sink.connection().expect("the sink has a connection");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "the sink moved to the new connection"
    );
    assert!(
        resumed_after < Duration::from_secs(3),
        "resumed {resumed_after:?} after the drop"
    );
    use crate::stream::sink::PacketSink as _;
    assert!(matches!(
        sink.poll_video_feedback().await.map(|f| f.kind),
        Some(crate::stream::sink::SinkVideoFeedbackKind::LocalIdrNeeded)
    ));
}

/// A link silent for 5 s (a cable out for longer) rejoins with a new
/// connection: the SFU fails a silent peer at 8 s, and libjuice cannot
/// restart ICE. The stream comes back in the same session.
#[tokio::test]
async fn a_long_silence_rejoins_with_a_new_connection() {
    let (port, _rpcs) = fake_nakama(&[], &[], Duration::ZERO).await;
    let (mut client, events) = client_for(port);
    let sfu = media_stream_sfu().await;
    live_host(&mut client, &sfu).await;
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    let driver = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        sfu.silence();
        wait_for_event(
            &events,
            "StreamReconnecting",
            Duration::from_secs(3),
            is_reconnecting_host,
        )
        .await;
        // The rejoin's new connection answers: the cable is back for it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while sfu.joins.lock().expect("joins").len() < 2 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no rejoin after 8 s of silence"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        wait_for_event(
            &events,
            "StreamResumed",
            Duration::from_secs(5),
            is_resumed_host,
        )
        .await;
        drop(cmd_tx);
    };
    tokio::time::timeout(LIMIT, async { tokio::join!(client.run(cmd_rx), driver) })
        .await
        .expect("the loop ends when the command channel closes");
    let joins = sfu.joins.lock().expect("joins").clone();
    assert!(
        joins.iter().all(|(s, r, _)| s == SESSION && r == "host"),
        "{joins:?}"
    );
    let gap = joins[1].2.duration_since(joins[0].2);
    assert!(
        gap >= Duration::from_secs(5),
        "the rejoin waited for 5 s of silence: {gap:?}"
    );
}

/// No rejoin within 30 s ends the stream with a message, and takes it back.
#[tokio::test]
async fn a_host_that_cannot_rejoin_ends_the_stream_with_a_message() {
    let (port, rpcs) = fake_nakama(&[], &[], Duration::ZERO).await;
    let (mut client, events) = client_for(port);
    let sfu = media_stream_sfu().await;
    live_host(&mut client, &sfu).await;

    client
        .apply_host_link_actions(vec![super::stream_link::LinkAction::GiveUp])
        .await;

    assert!(client.stream_session.is_none() && client.host_link.is_none());
    assert!(events.try_iter().any(|e| matches!(
        e,
        Event::StreamEnded { role: StreamRole::Host, ref reason, .. } if reason == "connection_lost"
    )));
    let deadline = tokio::time::Instant::now() + LIMIT;
    while rpc_calls(&rpcs, "stop_stream").is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no stop_stream");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Stage 6 gate: a 10 s Nakama RPC stall freezes neither END STREAM nor the
/// loop. The viewers get the end at once (the SFU `leave` goes before
/// `stop_stream`), and the loop takes the next command while `stop_stream`
/// waits. Before, END STREAM awaited `stop_stream` on the loop and sent the
/// leave after it: the viewers kept a frozen stream for the whole stall.
#[tokio::test]
async fn end_stream_and_the_loop_do_not_wait_on_a_stalled_backend() {
    let (port, rpcs) = fake_nakama(&[], &["stop_stream"], Duration::from_secs(10)).await;
    let (mut client, events) = client_for(port);
    let sfu = media_stream_sfu().await;
    live_host(&mut client, &sfu).await;
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    let driver = async {
        let clicked = Instant::now();
        cmd_tx.send(Command::StopStream).expect("the loop listens");
        wait_for_event(&events, "StreamEnded", Duration::from_secs(1), |e| {
            matches!(e, Event::StreamEnded { .. })
        })
        .await;
        let ended_after = clicked.elapsed();
        // The loop answers the next command while stop_stream stalls.
        cmd_tx
            .send(Command::ListAudioDevices)
            .expect("the loop listens");
        wait_for_event(&events, "AudioDevicesListed", Duration::from_secs(1), |e| {
            matches!(e, Event::AudioDevicesListed { .. })
        })
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while sfu.leaves.lock().expect("leaves").is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no SFU leave within 2 s of END STREAM"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(cmd_tx);
        ended_after
    };
    let (_, ended_after) =
        tokio::time::timeout(LIMIT, async { tokio::join!(client.run(cmd_rx), driver) })
            .await
            .expect("the loop ends when the command channel closes");
    assert!(ended_after < Duration::from_secs(1), "{ended_after:?}");
    assert_eq!(rpc_calls(&rpcs, "stop_stream").len(), 1);
}

/// The viewer keeps its leg and its last frame on `host_reconnecting`, and
/// a dropped viewer connection is a rejoin, not an end. Before stage 6 both
/// stopped the viewer.
#[tokio::test]
async fn a_viewer_keeps_watching_through_the_hosts_hold_and_its_own_drop() {
    let (port, _rpcs) = fake_nakama(&[], &[], Duration::ZERO).await;
    let (mut client, events) = client_for(port);
    client.viewer_link = Some(super::stream_watch::ViewerLinkState {
        generation: 3,
        link: StreamLink::new(StreamRole::Viewer),
        grant: StreamGrant::new("ws://127.0.0.1:9/ws".into(), "token".into()),
        session_id: SESSION.into(),
        connected_at: Instant::now(),
        attempt: None,
        token_task: None,
        token_tried_at: None,
        ticks: 0,
        rx_access_units: 0,
        rx_changed_at: Instant::now(),
        clicked_at: None,
    });

    let hold = SfuEvent::HostReconnecting {
        user_id: "host-1".into(),
        hold_ms: 30_000,
    };
    client.on_viewer_sfu_event(&hold).await;
    let seen: Vec<Event> = events.try_iter().collect();
    assert!(
        seen.iter().any(|e| matches!(
            e,
            Event::StreamReconnecting {
                role: StreamRole::Viewer
            }
        )),
        "{seen:?}"
    );
    assert!(!seen
        .iter()
        .any(|e| matches!(e, Event::StreamWatchingStopped)));

    let drop = SfuEvent::Disconnected {
        reason: "ws_closed".into(),
    };
    client.on_viewer_sfu_event(&drop).await;
    assert!(
        client
            .viewer_link
            .as_ref()
            .is_some_and(|s| s.link.is_rebuilding()),
        "a dropped connection rejoins"
    );
    assert!(!events
        .try_iter()
        .any(|e| matches!(e, Event::StreamWatchingStopped)));

    let ended = SfuEvent::Disconnected {
        reason: "session_ended".into(),
    };
    client.on_viewer_sfu_event(&ended).await;
    assert!(client.viewer_link.is_none());
    assert!(events.try_iter().any(|e| matches!(
        e,
        Event::StreamEnded { role: StreamRole::Viewer, ref reason, .. } if reason == "host_left"
    )));
}
