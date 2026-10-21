//! SFU voice join, run off the command loop.
//!
//! An SFU voice join opens a WebSocket, joins the voice session and waits up
//! to 5 s for the DataChannels. A refused connect alone takes about 2 s on
//! Windows. The command loop must not wait with it: voice ticks and every
//! other command stop while the loop waits. [`SfuVoiceJoins`] runs each
//! network step on a Tokio task and returns at once. The loop polls
//! [`SfuVoiceJoins::finished`] in its `select!` and continues the join with
//! the result.
//!
//! The join has three steps:
//!
//! | Step | Runs on | Work |
//! |---|---|---|
//! | 1 | task | WebSocket connect and welcome ([`SfuVoiceJoins::connect`]) |
//! | 2 | loop | Create the native peer. Only the loop owns the libmello context. |
//! | 3 | task | Join the session, wait for the DataChannels ([`SfuVoiceJoins::join_session`]) |
//!
//! One join runs at a time. A new join, a leave or a logout cancels the
//! running join. A cancelled join drops its connection and reports nothing.

use std::time::Duration;

use tokio::task::JoinHandle;

use crate::stream::StreamError;
use crate::transport::{PeerHandle, SfuConnection};

/// The voice session that an SFU join is for. The loop uses it to finish the
/// join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SfuVoiceJoin {
    pub crew_id: String,
    pub channel_id: String,
    pub local_id: String,
    /// An attempt of the reconnect scheduler.
    pub reconnect: bool,
}

/// A network step that ended.
pub enum JoinStep {
    /// Step 1 ended: the WebSocket is open. The loop creates the peer and
    /// starts step 3.
    Connected(SfuConnection),
    /// Step 3 ended: the session is joined and the DataChannels are open.
    Ready(SfuConnection),
}

/// A finished network step. `result` is `Err` when the step failed or its
/// task panicked.
pub struct JoinOutcome {
    pub join: SfuVoiceJoin,
    /// The step that ended, for logs: `"connect"` or `"voice join"`.
    pub step: &'static str,
    pub result: Result<JoinStep, String>,
}

struct Pending {
    join: SfuVoiceJoin,
    step: &'static str,
    task: JoinHandle<Result<JoinStep, StreamError>>,
}

/// The longest the WebSocket connect and welcome may take. A link that
/// drops packets (a pulled cable) never refuses: without a limit the step
/// waits for the OS TCP timeout, and no reconnect attempt can follow.
pub const SFU_CONNECT_STEP_LIMIT: Duration = Duration::from_secs(5);
/// The longest the voice join step may take: join, offer and answer, and
/// the DataChannel wait (itself at most 5 s).
pub const SFU_SESSION_STEP_LIMIT: Duration = Duration::from_secs(10);

/// The SFU voice join that runs now, if any. Owned by the command loop.
pub struct SfuVoiceJoins {
    pending: Option<Pending>,
    connect_limit: Duration,
    session_limit: Duration,
}

impl Default for SfuVoiceJoins {
    fn default() -> Self {
        Self {
            pending: None,
            connect_limit: SFU_CONNECT_STEP_LIMIT,
            session_limit: SFU_SESSION_STEP_LIMIT,
        }
    }
}

impl SfuVoiceJoins {
    /// Joins with other step limits, for tests.
    #[cfg(test)]
    pub(super) fn with_step_limits(connect: Duration, session: Duration) -> Self {
        Self {
            pending: None,
            connect_limit: connect,
            session_limit: session,
        }
    }
    /// Start step 1 on a task and return at once. Cancels the running join.
    ///
    /// Must be called inside a Tokio runtime.
    pub fn connect(&mut self, join: SfuVoiceJoin, endpoint: &str, token: &str) {
        let endpoint = endpoint.to_string();
        let token = token.to_string();
        let limit = self.connect_limit;
        self.spawn(join, "connect", limit, async move {
            SfuConnection::connect(&endpoint, &token)
                .await
                .map(JoinStep::Connected)
        });
    }

    /// Start step 3 on a task and return at once. `conn` and `peer` come from
    /// step 1 and step 2. Cancels the running join.
    ///
    /// Must be called inside a Tokio runtime.
    pub fn join_session(&mut self, join: SfuVoiceJoin, mut conn: SfuConnection, peer: PeerHandle) {
        let crew_id = join.crew_id.clone();
        let channel_id = join.channel_id.clone();
        let limit = self.session_limit;
        self.spawn(join, "voice join", limit, async move {
            conn.join_voice(peer, &crew_id, &channel_id).await?;
            conn.wait_for_datachannel_open().await?;
            Ok(JoinStep::Ready(conn))
        });
    }

    fn spawn<F>(&mut self, join: SfuVoiceJoin, step: &'static str, limit: Duration, work: F)
    where
        F: std::future::Future<Output = Result<JoinStep, StreamError>> + Send + 'static,
    {
        self.cancel();
        log::info!(
            "SFU voice {} for channel {} runs off the command loop",
            step,
            join.channel_id
        );
        let work = async move {
            match tokio::time::timeout(limit, work).await {
                Ok(result) => result,
                Err(_) => Err(StreamError::SfuConnectFailed(format!(
                    "{step} took longer than {limit:?}"
                ))),
            }
        };
        self.pending = Some(Pending {
            join,
            step,
            task: tokio::spawn(work),
        });
    }

    /// True while a join runs.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Stop the running join, if any. Its connection is dropped, and
    /// [`SfuVoiceJoins::finished`] does not report it.
    pub fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            log::info!(
                "SFU voice {} for channel {} cancelled",
                pending.step,
                pending.join.channel_id
            );
            pending.task.abort();
        }
    }

    /// Wait until the running step ends. With no join, this never completes,
    /// so it is safe as a `select!` branch.
    ///
    /// Cancel safe: if the `select!` takes another branch, the step continues
    /// and a later call returns its outcome.
    pub async fn finished(&mut self) -> JoinOutcome {
        let Some(pending) = self.pending.as_mut() else {
            return std::future::pending().await;
        };
        let joined = (&mut pending.task).await;
        let Pending { join, step, .. } = self
            .pending
            .take()
            .expect("the pending join is present until its outcome is taken");
        let result = match joined {
            Ok(Ok(next)) => Ok(next),
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(format!("task failed: {}", e)),
        };
        JoinOutcome { join, step, result }
    }
}

impl Drop for SfuVoiceJoins {
    fn drop(&mut self) {
        // A detached task would keep its connection open after the loop ends.
        self.cancel();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::time::Duration;

    const LIMIT: Duration = Duration::from_secs(10);

    pub(in crate::client) fn join_for(channel_id: &str) -> SfuVoiceJoin {
        SfuVoiceJoin {
            crew_id: "crew-1".into(),
            channel_id: channel_id.into(),
            local_id: "me".into(),
            reconnect: false,
        }
    }

    /// An SFU that takes the TCP connection and never answers the WebSocket
    /// handshake, so step 1 waits. Dropping `release` closes the connection,
    /// and step 1 fails.
    pub(in crate::client) struct SilentSfu {
        pub endpoint: String,
        /// Resolves when the client's TCP connection arrives.
        pub accepted: tokio::sync::oneshot::Receiver<()>,
        pub release: tokio::sync::oneshot::Sender<()>,
    }

    pub(in crate::client) async fn silent_sfu() -> SilentSfu {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let endpoint = format!(
            "ws://{}/ws",
            listener.local_addr().expect("listener has an address")
        );
        let (accepted_tx, accepted) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let _ = accepted_tx.send(());
            // Hold the socket open and silent until the test releases it.
            let _ = release_rx.await;
            drop(socket);
        });
        SilentSfu {
            endpoint,
            accepted,
            release,
        }
    }

    #[tokio::test]
    async fn connect_returns_while_the_sfu_does_not_answer() {
        let sfu = silent_sfu().await;
        let mut joins = SfuVoiceJoins::default();

        joins.connect(join_for("ch-1"), &sfu.endpoint, "token");
        assert!(joins.is_pending());
        tokio::time::timeout(LIMIT, sfu.accepted)
            .await
            .expect("the connect reaches the SFU")
            .expect("accept task runs");

        let early = tokio::time::timeout(Duration::from_millis(50), joins.finished()).await;
        assert!(early.is_err(), "the connect ended before the SFU answered");
        assert!(joins.is_pending(), "a timed-out poll must keep the join");

        drop(sfu.release);
        let outcome = tokio::time::timeout(LIMIT, joins.finished())
            .await
            .expect("the failure arrives");
        assert_eq!(outcome.join, join_for("ch-1"));
        assert_eq!(outcome.step, "connect");
        assert!(outcome.result.is_err(), "a closed socket fails the connect");
        assert!(!joins.is_pending());
    }

    #[tokio::test]
    async fn a_refused_connect_reports_the_failure() {
        // The dev log case: no SFU listens on the port.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local port");
            listener
                .local_addr()
                .expect("listener has an address")
                .port()
        };
        let mut joins = SfuVoiceJoins::default();
        joins.connect(join_for("ch-1"), &format!("ws://127.0.0.1:{port}/ws"), "t");

        let outcome = tokio::time::timeout(LIMIT, joins.finished())
            .await
            .expect("the failure arrives");
        assert_eq!(outcome.step, "connect");
        let err = outcome.result.err().expect("a refused connect fails");
        assert!(err.contains("SFU connection failed"), "{err}");
    }

    /// A connect that gets no answer ends at its limit, so the reconnect
    /// scheduler can try again (a pulled cable drops packets; nothing
    /// refuses the connection).
    #[tokio::test]
    async fn a_connect_without_an_answer_fails_at_its_limit() {
        let sfu = silent_sfu().await;
        let mut joins =
            SfuVoiceJoins::with_step_limits(Duration::from_millis(300), SFU_SESSION_STEP_LIMIT);
        let started = std::time::Instant::now();
        joins.connect(join_for("ch-1"), &sfu.endpoint, "token");

        let outcome = tokio::time::timeout(LIMIT, joins.finished())
            .await
            .expect("the step ends at its limit");
        assert_eq!(outcome.step, "connect");
        let err = outcome
            .result
            .err()
            .expect("a silent SFU fails the connect");
        assert!(err.contains("took longer than"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        drop(sfu.release);
    }

    #[tokio::test]
    async fn a_new_join_cancels_the_running_one() {
        let first = silent_sfu().await;
        let second = silent_sfu().await;
        let mut joins = SfuVoiceJoins::default();

        joins.connect(join_for("ch-old"), &first.endpoint, "t");
        joins.connect(join_for("ch-new"), &second.endpoint, "t");

        // The old join fails first, but it was cancelled: only the new one reports.
        drop(first.release);
        drop(second.release);
        let outcome = tokio::time::timeout(LIMIT, joins.finished())
            .await
            .expect("the new join reports");
        assert_eq!(outcome.join.channel_id, "ch-new");
    }

    #[tokio::test]
    async fn cancel_drops_the_join() {
        let sfu = silent_sfu().await;
        let mut joins = SfuVoiceJoins::default();
        joins.connect(join_for("ch-1"), &sfu.endpoint, "t");

        joins.cancel();
        assert!(!joins.is_pending());
        let polled = tokio::time::timeout(Duration::from_millis(50), joins.finished()).await;
        assert!(polled.is_err(), "a cancelled join must not report");
    }

    use crate::transport::SfuEvent;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    /// A real SDP answer for `offer`, from a native peer that lives only
    /// inside this call.
    fn native_answer(offer: &str) -> String {
        let id = std::ffi::CString::new("fake-sfu").expect("static id");
        let offer = std::ffi::CString::new(offer).expect("offer has no NUL");
        // SAFETY: the context argument is unused by libmello; the peer is
        // destroyed before return and the answer is copied first.
        unsafe {
            let peer = mello_sys::mello_peer_create(std::ptr::null_mut(), id.as_ptr());
            assert!(!peer.is_null(), "native answerer");
            let answer = mello_sys::mello_peer_create_answer(peer, offer.as_ptr());
            assert!(!answer.is_null(), "native answer");
            let text = std::ffi::CStr::from_ptr(answer)
                .to_string_lossy()
                .into_owned();
            mello_sys::mello_peer_destroy(peer);
            text
        }
    }

    fn text(value: serde_json::Value) -> Message {
        Message::Text(value.to_string())
    }

    /// An SFU that sends other signaling between the client's join and the
    /// `joined` reply, and between the offer and the `answer`, as a busy
    /// session does when members come and go during a join.
    async fn interleaving_sfu() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let endpoint = format!(
            "ws://{}/ws",
            listener.local_addr().expect("listener has an address")
        );
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            ws.send(text(serde_json::json!({
                "type": "welcome",
                "data": { "server_id": "fake", "region": "test" }
            })))
            .await
            .expect("send welcome");

            let join = loop {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) => break t,
                    Some(Ok(_)) => continue,
                    _ => panic!("no join message"),
                }
            };
            assert!(join.contains("join_voice"), "{join}");
            ws.send(text(serde_json::json!({
                "type": "member_joined",
                "data": { "user_id": "early-1", "role": "member" }
            })))
            .await
            .expect("send member_joined");
            ws.send(text(serde_json::json!({
                "type": "joined",
                "data": { "session_type": "voice", "session_id": "voice:crew-1:ch-1", "members": [] }
            })))
            .await
            .expect("send joined");

            let offer = loop {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) => {
                        let v: serde_json::Value =
                            serde_json::from_str(&t).expect("client sends JSON");
                        if v["type"] == "offer" {
                            break v["data"]["sdp"].as_str().expect("offer sdp").to_string();
                        }
                    }
                    Some(Ok(_)) => continue,
                    _ => panic!("no offer"),
                }
            };
            let answer = native_answer(&offer);
            for msg in [
                serde_json::json!({
                    "type": "member_joined",
                    "data": { "user_id": "early-2", "role": "member" }
                }),
                serde_json::json!({
                    "type": "ice_candidate",
                    "data": {
                        "candidate": "candidate:1 1 UDP 2130706431 127.0.0.1 9 typ host",
                        "sdp_mid": "0",
                        "sdp_mline_index": 0
                    }
                }),
                serde_json::json!({
                    "type": "member_left",
                    "data": { "user_id": "early-2", "reason": "left" }
                }),
                serde_json::json!({ "type": "answer", "data": { "sdp": answer } }),
            ] {
                ws.send(text(msg)).await.expect("send");
            }
            // Keep the socket open; the client's ICE candidates arrive here.
            while ws.next().await.is_some() {}
        });
        endpoint
    }

    /// Signaling that arrives before `joined` or before `answer` must not
    /// fail the join, and must reach the event queue in order once the join
    /// completes.
    #[tokio::test]
    async fn signaling_before_joined_and_answer_is_kept_for_the_listener() {
        let endpoint = interleaving_sfu().await;
        let mut conn = tokio::time::timeout(LIMIT, SfuConnection::connect(&endpoint, "token"))
            .await
            .expect("connect in time")
            .expect("connect");
        // SAFETY: a null context is allowed.
        let peer = unsafe { SfuConnection::create_peer(std::ptr::null_mut(), &[]) }.expect("peer");

        let session = tokio::time::timeout(LIMIT, conn.join_voice(peer, "crew-1", "ch-1"))
            .await
            .expect("join in time")
            .expect("the join succeeds");
        assert_eq!(session.session_id, "voice:crew-1:ch-1");

        let mut seen = Vec::new();
        while seen.len() < 3 {
            let ev = tokio::time::timeout(LIMIT, conn.recv_event())
                .await
                .expect("queued signaling arrives")
                .expect("event channel open");
            match ev {
                SfuEvent::MemberJoined { user_id, .. } => seen.push(format!("joined:{user_id}")),
                SfuEvent::MemberLeft { user_id, .. } => seen.push(format!("left:{user_id}")),
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(seen, ["joined:early-1", "joined:early-2", "left:early-2"]);
    }

    /// An SFU that completes the join, then closes the WebSocket with a
    /// normal close frame when `close` fires. With `ack_leave` it first
    /// sends `left`, as the SFU does after a client `leave`.
    async fn closing_sfu(close: tokio::sync::oneshot::Receiver<()>, ack_leave: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let endpoint = format!(
            "ws://{}/ws",
            listener.local_addr().expect("listener has an address")
        );
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            ws.send(text(serde_json::json!({
                "type": "welcome",
                "data": { "server_id": "fake", "region": "test" }
            })))
            .await
            .expect("send welcome");
            loop {
                let Some(Ok(Message::Text(t))) = ws.next().await else {
                    return;
                };
                let v: serde_json::Value = serde_json::from_str(&t).expect("client sends JSON");
                match v["type"].as_str() {
                    Some("join_voice") => ws
                        .send(text(serde_json::json!({
                            "type": "joined",
                            "data": { "session_type": "voice", "session_id": "voice:crew-1:ch-1", "members": [] }
                        })))
                        .await
                        .expect("send joined"),
                    Some("offer") => {
                        let answer = native_answer(v["data"]["sdp"].as_str().expect("sdp"));
                        ws.send(text(serde_json::json!({ "type": "answer", "data": { "sdp": answer } })))
                            .await
                            .expect("send answer");
                        break;
                    }
                    _ => {}
                }
            }
            let _ = close.await;
            if ack_leave {
                let _ = ws
                    .send(text(
                        serde_json::json!({ "type": "left", "data": { "reason": "client_leave" } }),
                    ))
                    .await;
            }
            // A clean close handshake: the client's read then ends with no
            // error. Hold the TCP connection until the client drops it.
            let _ = ws.close(None).await;
            while ws.next().await.is_some() {}
            tokio::time::sleep(LIMIT).await;
        });
        endpoint
    }

    async fn joined_connection(endpoint: &str) -> SfuConnection {
        let mut conn = tokio::time::timeout(LIMIT, SfuConnection::connect(endpoint, "token"))
            .await
            .expect("connect in time")
            .expect("connect");
        // SAFETY: a null context is allowed.
        let peer = unsafe { SfuConnection::create_peer(std::ptr::null_mut(), &[]) }.expect("peer");
        tokio::time::timeout(LIMIT, conn.join_voice(peer, "crew-1", "ch-1"))
            .await
            .expect("join in time")
            .expect("the join succeeds");
        conn
    }

    /// Spec 10 section 7.6: the SFU closing the signaling socket ends the
    /// session (the SFU removes the peer). The client reports it at once, so
    /// the call shows "reconnecting" without waiting for the liveness check.
    #[tokio::test]
    async fn a_server_close_of_the_signaling_socket_is_a_disconnect() {
        let (close_tx, close) = tokio::sync::oneshot::channel();
        let endpoint = closing_sfu(close, false).await;
        let conn = joined_connection(&endpoint).await;

        let started = std::time::Instant::now();
        let _ = close_tx.send(());
        let ev = tokio::time::timeout(LIMIT, conn.recv_event())
            .await
            .expect("an event in time")
            .expect("event channel open");
        assert!(
            matches!(ev, SfuEvent::Disconnected { .. }),
            "expected Disconnected, got {ev:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    /// The close after the SFU's `left` (the reply to our `leave`) is not
    /// a drop.
    #[tokio::test]
    async fn the_close_after_left_is_not_a_disconnect() {
        let (close_tx, close) = tokio::sync::oneshot::channel();
        let endpoint = closing_sfu(close, true).await;
        let conn = joined_connection(&endpoint).await;

        let _ = close_tx.send(());
        let ev = tokio::time::timeout(Duration::from_millis(500), conn.recv_event()).await;
        assert!(
            !matches!(ev, Ok(Some(SfuEvent::Disconnected { .. }))),
            "a leave ack raised {ev:?}"
        );
    }

    /// The mapped address the fake STUN server reports: a server reflexive
    /// candidate with it can only come from that server's late answer.
    const MAPPED_IP: [u8; 4] = [203, 0, 113, 7];
    const MAPPED_PORT: u16 = 40_000;

    /// A STUN server on a local UDP port that answers a Binding request
    /// only after `release` fires, with the mapped address above. The
    /// client's server reflexive candidate then appears after that moment.
    async fn late_stun_server(release: tokio::sync::oneshot::Receiver<()>) -> u16 {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind a local UDP port");
        let port = socket.local_addr().expect("socket has an address").port();
        tokio::spawn(async move {
            let mut release = release;
            let mut released = false;
            let mut last: Option<([u8; 12], std::net::SocketAddr)> = None;
            let mut buf = [0u8; 1500];
            loop {
                tokio::select! {
                    got = socket.recv_from(&mut buf) => {
                        let Ok((n, from)) = got else { return };
                        // Binding request with the RFC 5389 magic cookie.
                        if n < 20 || buf[0..2] != [0x00, 0x01] || buf[4..8] != [0x21, 0x12, 0xA4, 0x42] {
                            continue;
                        }
                        let mut tid = [0u8; 12];
                        tid.copy_from_slice(&buf[8..20]);
                        last = Some((tid, from));
                    }
                    _ = &mut release, if !released => {
                        released = true;
                    }
                }
                if released {
                    if let Some((tid, to)) = last.take() {
                        let mut resp = vec![0x01, 0x01, 0x00, 0x0C, 0x21, 0x12, 0xA4, 0x42];
                        resp.extend_from_slice(&tid);
                        // XOR-MAPPED-ADDRESS, IPv4.
                        resp.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
                        resp.extend_from_slice(&(MAPPED_PORT ^ 0x2112).to_be_bytes());
                        let cookie = [0x21, 0x12, 0xA4, 0x42];
                        resp.extend(MAPPED_IP.iter().zip(cookie).map(|(a, c)| a ^ c));
                        let _ = socket.send_to(&resp, to).await;
                    }
                }
            }
        });
        port
    }

    /// A fake SFU that answers the join and the offer like the real one, and
    /// records the type of each client message in arrival order. A
    /// candidate that names the late STUN mapped address goes to `late`.
    async fn recording_sfu(
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        answered: tokio::sync::oneshot::Sender<()>,
        late: tokio::sync::oneshot::Sender<String>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let endpoint = format!(
            "ws://{}/ws",
            listener.local_addr().expect("listener has an address")
        );
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            ws.send(text(serde_json::json!({
                "type": "welcome",
                "data": { "server_id": "fake", "region": "test" }
            })))
            .await
            .expect("send welcome");
            let mut answered = Some(answered);
            let mut late = Some(late);
            let mapped = format!(
                "{}.{}.{}.{}",
                MAPPED_IP[0], MAPPED_IP[1], MAPPED_IP[2], MAPPED_IP[3]
            );
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(t) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).expect("client sends JSON");
                let kind = v["type"].as_str().unwrap_or("").to_string();
                log.lock().expect("log").push(kind.clone());
                match kind.as_str() {
                    "join_voice" => {
                        ws.send(text(serde_json::json!({
                            "type": "joined",
                            "data": { "session_type": "voice", "session_id": "voice:crew-1:ch-1", "members": [] }
                        })))
                        .await
                        .expect("send joined");
                    }
                    "offer" => {
                        let offer = v["data"]["sdp"].as_str().expect("offer sdp");
                        let answer = native_answer(offer);
                        ws.send(text(
                            serde_json::json!({ "type": "answer", "data": { "sdp": answer } }),
                        ))
                        .await
                        .expect("send answer");
                        if let Some(tx) = answered.take() {
                            let _ = tx.send(());
                        }
                    }
                    "ice_candidate" => {
                        let candidate = v["data"]["candidate"].as_str().unwrap_or("").to_string();
                        if candidate.contains(&mapped) {
                            if let Some(tx) = late.take() {
                                let _ = tx.send(candidate);
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
        endpoint
    }

    /// mello-sfu 01-SFU.md section 5.5: the SFU refuses an `ice_candidate`
    /// that arrives before the `offer` with INVALID_REQUEST. The client keeps
    /// every local candidate until the offer is on the wire, also one that
    /// libdatachannel emits while `create_offer` runs. A candidate found
    /// after the answer (here a server reflexive one from a slow STUN
    /// server, as a TURN relay candidate can be) still reaches the SFU.
    #[tokio::test]
    async fn local_candidates_follow_the_offer_and_late_ones_still_reach_the_sfu() {
        let (release_stun, stun_release) = tokio::sync::oneshot::channel();
        let stun_port = late_stun_server(stun_release).await;
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (answered_tx, answered) = tokio::sync::oneshot::channel();
        let (late_tx, late) = tokio::sync::oneshot::channel();
        let endpoint = recording_sfu(std::sync::Arc::clone(&log), answered_tx, late_tx).await;

        let mut conn = tokio::time::timeout(LIMIT, SfuConnection::connect(&endpoint, "token"))
            .await
            .expect("connect in time")
            .expect("connect");
        let ice = [format!("stun:127.0.0.1:{stun_port}")];
        // SAFETY: a null context is allowed.
        let peer = unsafe { SfuConnection::create_peer(std::ptr::null_mut(), &ice) }.expect("peer");
        tokio::time::timeout(LIMIT, conn.join_voice(peer, "crew-1", "ch-1"))
            .await
            .expect("join in time")
            .expect("the join succeeds");

        // The answer is out: now the STUN server answers, and the client
        // finds its server reflexive candidate.
        tokio::time::timeout(LIMIT, answered)
            .await
            .expect("the SFU answered")
            .expect("answer signal");
        let _ = release_stun.send(());
        let candidate = tokio::time::timeout(LIMIT, late)
            .await
            .expect("the late candidate reached the SFU")
            .expect("candidate signal");
        assert!(candidate.contains("typ srflx"), "{candidate}");

        let log = log.lock().expect("log").clone();
        let offer_at = log
            .iter()
            .position(|t| t == "offer")
            .expect("the client sent an offer");
        let first_candidate = log
            .iter()
            .position(|t| t == "ice_candidate")
            .expect("the client sent candidates");
        assert_eq!(log[0], "join_voice", "{log:?}");
        assert!(
            offer_at < first_candidate,
            "an ice_candidate went out before the offer: {log:?}"
        );
        drop(conn);
    }

    /// A fake SFU that answers the join and the offer, then announces one
    /// passive ICE-TCP candidate on `tcp_port`, as mello-sfu does for
    /// `SFU_ICE_TCP_PORT` (01-SFU.md section 3.1).
    async fn ice_tcp_sfu(tcp_port: u16) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let endpoint = format!(
            "ws://{}/ws",
            listener.local_addr().expect("listener has an address")
        );
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the client connects");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            ws.send(text(serde_json::json!({
                "type": "welcome",
                "data": { "server_id": "fake", "region": "test" }
            })))
            .await
            .expect("send welcome");
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(t) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).expect("client sends JSON");
                match v["type"].as_str() {
                    Some("join_voice") => ws
                        .send(text(serde_json::json!({
                            "type": "joined",
                            "data": { "session_type": "voice", "session_id": "voice:crew-1:ch-1", "members": [] }
                        })))
                        .await
                        .expect("send joined"),
                    Some("offer") => {
                        let offer = v["data"]["sdp"].as_str().expect("offer sdp");
                        let mid = offer
                            .lines()
                            .find_map(|l| l.strip_prefix("a=mid:"))
                            .expect("the offer has a mid")
                            .trim()
                            .to_string();
                        let answer = native_answer(offer);
                        ws.send(text(
                            serde_json::json!({ "type": "answer", "data": { "sdp": answer } }),
                        ))
                        .await
                        .expect("send answer");
                        ws.send(text(serde_json::json!({
                            "type": "ice_candidate",
                            "data": {
                                "candidate": format!(
                                    "candidate:1 1 TCP 1518280447 127.0.0.1 {tcp_port} typ host tcptype passive"
                                ),
                                "sdp_mid": mid,
                                "sdp_mline_index": 0
                            }
                        })))
                        .await
                        .expect("send the passive TCP candidate");
                    }
                    _ => {}
                }
            }
        });
        endpoint
    }

    /// Active ICE-TCP (RFC 6544): given the SFU's passive TCP candidate, the
    /// client connects to it and sends a STUN Binding request in RFC 4571
    /// framing. This is the path a network that blocks UDP takes.
    #[tokio::test]
    async fn the_client_runs_ice_checks_over_tcp_to_a_passive_candidate() {
        use tokio::io::AsyncReadExt;

        let ice_tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local TCP port");
        let tcp_port = ice_tcp
            .local_addr()
            .expect("listener has an address")
            .port();
        let endpoint = ice_tcp_sfu(tcp_port).await;
        let conn = joined_connection(&endpoint).await;

        let (mut socket, _) = tokio::time::timeout(LIMIT, ice_tcp.accept())
            .await
            .expect("the client connects to the passive TCP candidate")
            .expect("accept");
        let mut len = [0u8; 2];
        tokio::time::timeout(LIMIT, socket.read_exact(&mut len))
            .await
            .expect("a framed packet in time")
            .expect("read the RFC 4571 length");
        let mut packet = vec![0u8; u16::from_be_bytes(len) as usize];
        tokio::time::timeout(LIMIT, socket.read_exact(&mut packet))
            .await
            .expect("the packet in time")
            .expect("read the packet");
        assert!(packet.len() >= 20, "a STUN message: {packet:?}");
        assert_eq!(packet[0..2], [0x00, 0x01], "a Binding request");
        assert_eq!(packet[4..8], [0x21, 0x12, 0xA4, 0x42], "the magic cookie");
        drop(conn);
    }
}
