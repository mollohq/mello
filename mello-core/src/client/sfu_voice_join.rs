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

use tokio::task::JoinHandle;

use crate::stream::StreamError;
use crate::transport::{PeerHandle, SfuConnection};

/// The voice session that an SFU join is for. The loop uses it to finish the
/// join, or to fall back to P2P when the join fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SfuVoiceJoin {
    pub crew_id: String,
    pub channel_id: String,
    pub local_id: String,
    /// The channel members other than the local user. The P2P fallback
    /// connects to them.
    pub p2p_peer_ids: Vec<String>,
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

/// The SFU voice join that runs now, if any. Owned by the command loop.
#[derive(Default)]
pub struct SfuVoiceJoins {
    pending: Option<Pending>,
}

impl SfuVoiceJoins {
    /// Start step 1 on a task and return at once. Cancels the running join.
    ///
    /// Must be called inside a Tokio runtime.
    pub fn connect(&mut self, join: SfuVoiceJoin, endpoint: &str, token: &str) {
        let endpoint = endpoint.to_string();
        let token = token.to_string();
        self.spawn(join, "connect", async move {
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
        self.spawn(join, "voice join", async move {
            conn.join_voice(peer, &crew_id, &channel_id).await?;
            conn.wait_for_datachannel_open().await?;
            Ok(JoinStep::Ready(conn))
        });
    }

    fn spawn<F>(&mut self, join: SfuVoiceJoin, step: &'static str, work: F)
    where
        F: std::future::Future<Output = Result<JoinStep, StreamError>> + Send + 'static,
    {
        self.cancel();
        log::info!(
            "SFU voice {} for channel {} runs off the command loop",
            step,
            join.channel_id
        );
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
            p2p_peer_ids: vec!["peer-1".into()],
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
}
