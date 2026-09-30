//! End-to-end tests against a live Nakama stack.
//!
//! Fills the largest hole in the suite: **not one Go RPC handler or hook is
//! covered by a test.** All 106 backend tests are pure functions — nothing
//! exercises a registered RPC, a before/after hook, or a payload shape. A
//! renamed field, a changed error code, or a broken hook passes every existing
//! check and only fails against a real server.
//!
//! The RPC contract test (`mello-core/tests/rpc_contract.rs`) catches renamed
//! *names* statically; these catch changed *behaviour*.
//!
//! Requires a running backend. Start one with:
//!
//! ```text
//! ./scripts/e2e.sh
//! ```
//!
//! Tests skip (loudly) when `MELLO_E2E` is unset, so `cargo test --workspace`
//! stays hermetic and fast for everyone else.

use mello_core::nakama::client::NakamaClient;
use mello_core::Config;

/// Local dev stack, overridable through the usual `NAKAMA_*` variables.
fn e2e_config() -> Config {
    Config::development().with_env_overrides()
}

/// Whether the caller asked for e2e tests.
///
/// Returns false rather than failing so the suite stays green on a machine
/// with no backend — but every skip prints, so a silently-empty e2e run is
/// visible rather than looking like success.
fn e2e_enabled(test_name: &str) -> bool {
    if std::env::var("MELLO_E2E").is_ok() {
        return true;
    }
    println!("SKIP {test_name}: set MELLO_E2E=1 and run ./scripts/e2e.sh for a live backend");
    false
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn random_device_id() -> String {
    format!("e2e-{:032x}", rand::random::<u128>())
}

/// The health RPC also carries the protocol version the client checks on
/// connect, so a mismatch here means every client refuses to talk to this
/// server.
#[test]
fn health_rpc_reports_a_compatible_protocol() {
    if !e2e_enabled("health_rpc_reports_a_compatible_protocol") {
        return;
    }

    rt().block_on(async {
        let mut client = NakamaClient::new(e2e_config());
        client
            .authenticate_device(&random_device_id())
            .await
            .expect("device auth");

        let raw = client
            .rpc("health", &serde_json::json!({}))
            .await
            .expect("health rpc");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("health returns JSON");

        assert!(
            body.get("protocol_version").is_some(),
            "health must report protocol_version; the client gates connection on \
             it. Got: {body}"
        );
    });
}

/// Guest discovery is the first call a new user makes, and the one that took
/// signup down. Exercised here against a real server so a change to the
/// handler's response shape is caught locally rather than in production.
#[test]
fn guest_discovery_works_without_a_session() {
    if !e2e_enabled("guest_discovery_works_without_a_session") {
        return;
    }

    rt().block_on(async {
        // Deliberately no authentication: this must work for a brand-new user
        // who has never had a session, using only the http_key.
        let client = NakamaClient::new(e2e_config());
        let (crews, _cursor) = client
            .discover_crews_public(50, None)
            .await
            .expect("guest discovery must work with only the http_key");

        // Seeded stacks have crews; an empty result is legal but degrades
        // onboarding to create-only, so say so rather than asserting.
        println!("discovered {} crews", crews.len());
    });
}

/// Full signup: create an account, then create a crew through the real
/// `create_crew` RPC (which also provisions a default voice channel and an
/// invite code, and fires the AfterJoinCrew hook).
#[test]
fn signup_and_crew_creation_round_trip() {
    if !e2e_enabled("signup_and_crew_creation_round_trip") {
        return;
    }

    rt().block_on(async {
        let mut client = NakamaClient::new(e2e_config());

        let (user, created) = client
            .authenticate_device(&random_device_id())
            .await
            .expect("device auth");
        assert!(created, "a fresh device id must create a new account");
        assert!(!user.id.is_empty(), "the new account must have an id");

        let crew_name = format!("E2E Crew {:x}", rand::random::<u32>());
        let raw = client
            .rpc(
                "create_crew",
                &serde_json::json!({
                    "name": crew_name,
                    "description": "created by the e2e suite",
                    "open": false,
                }),
            )
            .await
            .expect("create_crew");

        let body: serde_json::Value = serde_json::from_str(&raw).expect("create_crew returns JSON");
        let crew_id = body
            .get("crew_id")
            .or_else(|| body.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| {
                panic!("create_crew response has no crew id. Shape changed? Got: {body}")
            });
        assert!(!crew_id.is_empty());

        // Clean up so repeated local runs do not accumulate state.
        let _ = client.delete_account().await;
    });
}

/// Signing in with an unknown email must fail, not quietly create an account.
///
/// This used to send `create=true`, so a typo on the sign-in panel produced a
/// brand-new account with no crew, nickname or avatar — the user saw an empty
/// app and assumed their crews were gone. Asserted against a live server
/// because the behaviour lives in a query parameter, where a unit test would
/// only be re-reading the same string.
#[test]
fn signing_in_with_an_unknown_email_is_refused() {
    if !e2e_enabled("signing_in_with_an_unknown_email_is_refused") {
        return;
    }

    rt().block_on(async {
        let mut client = NakamaClient::new(e2e_config());
        let email = format!("nobody-{:032x}@example.invalid", rand::random::<u128>());

        let result = client.login_email(&email, "irrelevant-password").await;

        assert!(
            result.is_err(),
            "signing in with an address that has no account must fail; \
             creating one here bypasses onboarding entirely"
        );
    });
}

/// ...and a real account can still sign in, so the above is not just breaking
/// login outright.
#[test]
fn an_existing_email_identity_can_still_sign_in() {
    if !e2e_enabled("an_existing_email_identity_can_still_sign_in") {
        return;
    }

    rt().block_on(async {
        let email = format!("e2e-{:032x}@example.invalid", rand::random::<u128>());
        let password = "correct-horse-battery";

        // Create the identity the way onboarding does: device account first,
        // then link an email to it.
        let mut client = NakamaClient::new(e2e_config());
        client
            .authenticate_device(&random_device_id())
            .await
            .expect("device auth");
        client
            .link_email(&email, password)
            .await
            .expect("link email to the device account");

        // A fresh client, as a returning user on another launch.
        let mut returning = NakamaClient::new(e2e_config());
        let user = returning
            .login_email(&email, password)
            .await
            .expect("a linked email identity must still sign in with create=false");
        assert!(!user.id.is_empty());

        let _ = client.delete_account().await;
    });
}

/// `voice_join` decides SFU vs P2P, enforces capacity and signs an SFU token.
/// None of that logic is covered by the Go unit tests at the RPC level.
#[test]
fn voice_join_returns_a_usable_room() {
    if !e2e_enabled("voice_join_returns_a_usable_room") {
        return;
    }

    rt().block_on(async {
        let mut client = NakamaClient::new(e2e_config());
        client
            .authenticate_device(&random_device_id())
            .await
            .expect("device auth");

        let crew_name = format!("E2E Voice {:x}", rand::random::<u32>());
        let raw = client
            .rpc(
                "create_crew",
                &serde_json::json!({ "name": crew_name, "open": false }),
            )
            .await
            .expect("create_crew");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("JSON");
        let crew_id = body
            .get("crew_id")
            .or_else(|| body.get("id"))
            .and_then(|v| v.as_str())
            .expect("crew id")
            .to_string();

        let raw = client
            .rpc("voice_join", &serde_json::json!({ "crew_id": crew_id }))
            .await
            .expect("voice_join on a crew we just created and own");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("JSON");

        assert!(
            body.get("mode").is_some() || body.get("channel_id").is_some(),
            "voice_join must say which channel/mode was joined; the client \
             branches on it to pick SFU or P2P. Got: {body}"
        );

        let _ = client.delete_account().await;
    });
}

/// Joining a crew through an invite link.
///
/// `join_by_invite_code` passed an empty username to `GroupUserJoin`, which
/// Nakama 3.21 rejects with "expects a username string". Every invite join
/// failed, and the only other join path (Discover) uses Nakama's built-in
/// group join, so nothing else covered it.
#[test]
fn a_second_user_can_join_a_crew_by_invite_code() {
    if !e2e_enabled("a_second_user_can_join_a_crew_by_invite_code") {
        return;
    }

    rt().block_on(async {
        // User A creates an open crew and shares a fresh invite code.
        let mut owner = NakamaClient::new(e2e_config());
        owner
            .authenticate_device(&random_device_id())
            .await
            .expect("device auth for the owner");
        let crew_name = format!("E2E Invite {:x}", rand::random::<u32>());
        let (crew, _) = owner
            .create_crew(&crew_name, "", true, None, &[])
            .await
            .expect("create_crew");
        let code = owner
            .create_invite_code(&crew.id)
            .await
            .expect("create_invite_code");

        // User B signs up and follows the link.
        let mut joiner = NakamaClient::new(e2e_config());
        let (joiner_user, _) = joiner
            .authenticate_device(&random_device_id())
            .await
            .expect("device auth for the joiner");

        let (joined_id, joined_name) = joiner
            .join_by_invite_code(&code)
            .await
            .expect("join_by_invite_code must succeed for a valid code");
        assert_eq!(joined_id, crew.id, "the RPC must return the invited crew");
        assert_eq!(joined_name, crew_name);

        let joiner_crews = joiner.list_user_groups().await.expect("list_user_groups");
        assert!(
            joiner_crews.iter().any(|c| c.id == crew.id),
            "the joiner must now be in the crew. Got: {joiner_crews:?}"
        );
        let members = owner
            .list_group_users(&crew.id)
            .await
            .expect("list_group_users");
        assert!(
            members.iter().any(|m| m.id == joiner_user.id),
            "the owner must see the joiner as a member. Got: {members:?}"
        );
        assert_eq!(
            crew_member_state(&owner, &crew.id, &joiner_user.id).await,
            Some(GROUP_STATE_MEMBER),
            "the joiner must be a member, not a join request"
        );

        // Following the same link again is not an error: the client opens the
        // crew.
        let (again_id, _) = joiner
            .join_by_invite_code(&code)
            .await
            .expect("an existing member following the link again must succeed");
        assert_eq!(again_id, crew.id);

        let _ = joiner.delete_account().await;
        let _ = owner.delete_account().await;
    });
}

/// Nakama `group_edge` states the invite tests assert on.
const GROUP_STATE_MEMBER: i64 = 2;
const GROUP_STATE_JOIN_REQUEST: i64 = 3;

/// The caller's view of `user_id`'s edge state in the crew, or `None` when the
/// user has no edge.
///
/// Nakama lists a join request next to the real members, so a membership
/// check that only looks for the user ID passes for a pending request.
/// `crew_state_get` reports each member's edge state as `role`.
async fn crew_member_state(viewer: &NakamaClient, crew_id: &str, user_id: &str) -> Option<i64> {
    let raw = viewer
        .rpc("crew_state_get", &serde_json::json!({ "crew_id": crew_id }))
        .await
        .expect("crew_state_get");
    let body: serde_json::Value = serde_json::from_str(&raw).expect("crew_state_get returns JSON");
    let members = body
        .get("members")
        .and_then(|m| m.as_array())
        .unwrap_or_else(|| panic!("crew_state_get has no members list. Got: {body}"));
    members
        .iter()
        .find(|m| m.get("user_id").and_then(|v| v.as_str()) == Some(user_id))
        .map(|m| {
            m.get("role")
                .and_then(|v| v.as_i64())
                .unwrap_or_else(|| panic!("member has no role. Got: {m}"))
        })
}

/// A new owner with a crew and a fresh invite code for it.
async fn crew_with_invite(open: bool) -> (NakamaClient, String, String) {
    let mut owner = NakamaClient::new(e2e_config());
    owner
        .authenticate_device(&random_device_id())
        .await
        .expect("device auth for the owner");
    let crew_name = format!("E2E Invite {:x}", rand::random::<u32>());
    let (crew, _) = owner
        .create_crew(&crew_name, "", open, None, &[])
        .await
        .expect("create_crew");
    let code = owner
        .create_invite_code(&crew.id)
        .await
        .expect("create_invite_code");
    (owner, crew.id, code)
}

/// Joining a private crew through an invite link.
///
/// New crews are private (a closed Nakama group). `join_by_invite_code` called
/// `GroupUserJoin`, which only files a join request for a closed group. The
/// RPC reported success, but no one was asked to approve the request, and the
/// crew chat refused the user as a non-member (issue #83).
#[test]
fn a_second_user_can_join_a_private_crew_by_invite_code() {
    if !e2e_enabled("a_second_user_can_join_a_private_crew_by_invite_code") {
        return;
    }

    rt().block_on(async {
        let (owner, crew_id, code) = crew_with_invite(false).await;

        let mut joiner = NakamaClient::new(e2e_config());
        let (joiner_user, _) = joiner
            .authenticate_device(&random_device_id())
            .await
            .expect("device auth for the joiner");

        let (joined_id, _) = joiner
            .join_by_invite_code(&code)
            .await
            .expect("join_by_invite_code must succeed for a valid code");
        assert_eq!(joined_id, crew_id, "the RPC must return the invited crew");
        assert_eq!(
            crew_member_state(&owner, &crew_id, &joiner_user.id).await,
            Some(GROUP_STATE_MEMBER),
            "a valid invite code must make the joiner a member of a private crew, \
             not a join request"
        );

        let (again_id, _) = joiner
            .join_by_invite_code(&code)
            .await
            .expect("an existing member following the link again must succeed");
        assert_eq!(again_id, crew_id);

        let _ = joiner.delete_account().await;
        let _ = owner.delete_account().await;
    });
}

/// An invite code completes a join request the user filed before.
///
/// A user who asked to join a private crew through Nakama's own group join
/// has a pending request. The invite code is the authorization, so following
/// it must make that user a member.
#[test]
fn an_invite_code_completes_a_pending_join_request() {
    if !e2e_enabled("an_invite_code_completes_a_pending_join_request") {
        return;
    }

    rt().block_on(async {
        let (owner, crew_id, code) = crew_with_invite(false).await;

        let mut joiner = NakamaClient::new(e2e_config());
        let (joiner_user, _) = joiner
            .authenticate_device(&random_device_id())
            .await
            .expect("device auth for the joiner");

        joiner
            .join_group(&crew_id)
            .await
            .expect("Nakama group join on a private crew files a request");
        assert_eq!(
            crew_member_state(&owner, &crew_id, &joiner_user.id).await,
            Some(GROUP_STATE_JOIN_REQUEST),
            "precondition: the joiner must hold a pending join request"
        );

        joiner
            .join_by_invite_code(&code)
            .await
            .expect("join_by_invite_code must succeed for a pending request");
        assert_eq!(
            crew_member_state(&owner, &crew_id, &joiner_user.id).await,
            Some(GROUP_STATE_MEMBER),
            "the invite code must turn the join request into a membership"
        );

        let _ = joiner.delete_account().await;
        let _ = owner.delete_account().await;
    });
}

/// Push tokens (spec 23): the core's register and unregister calls reach the
/// real handlers, a repeat register is an upsert, and invalid input is refused
/// rather than stored. The rows are server-only, so storage and the reassign
/// rule are checked by the Go tests and a manual SQL check, not here.
#[test]
fn push_token_register_and_unregister_round_trip() {
    if !e2e_enabled("push_token_register_and_unregister_round_trip") {
        return;
    }

    rt().block_on(async {
        let mut client = NakamaClient::new(e2e_config());
        client
            .authenticate_device(&random_device_id())
            .await
            .expect("device auth");

        client
            .register_push_token("ab12cd34", "ios", "sandbox")
            .await
            .expect("register");
        client
            .register_push_token("ab12cd34", "ios", "sandbox")
            .await
            .expect("a repeat register is an upsert");
        client
            .register_push_token("not-hex", "ios", "production")
            .await
            .expect_err("a non-hex iOS token must be refused");
        client
            .register_push_token("ab12cd34", "ios", "staging")
            .await
            .expect_err("an unknown APNs environment must be refused");
        client
            .unregister_push_token("ab12cd34")
            .await
            .expect("unregister");

        let _ = client.delete_account().await;
    });
}

/// One request captured by the stub push Worker.
struct CapturedSend {
    head: String,
    body: serde_json::Value,
    at: std::time::Instant,
}

/// Serves `POST /send` on the host port that `scripts/e2e.sh` gave Nakama as
/// `PUSH_WORKER_URL`, answers like the real Worker, and reports each request.
fn stub_push_worker(port: u16) -> std::sync::mpsc::Receiver<CapturedSend> {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind(("0.0.0.0", port)).expect("bind stub push worker");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut head = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut raw = vec![0u8; len];
            let _ = reader.read_exact(&mut raw);
            let body: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_default();
            let token = body["tokens"][0]["token"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let reply = serde_json::json!({
                "results": [{"token": token, "status": "sent", "apns_id": "e2e"}],
                "prune": [],
            })
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            if tx
                .send(CapturedSend {
                    head,
                    body,
                    at: std::time::Instant::now(),
                })
                .is_err()
            {
                return;
            }
        }
    });
    rx
}

/// Waits for the next `/send` without blocking the single-threaded runtime:
/// the socket writer task must run to deliver messages at all.
async fn next_send(rx: &std::sync::mpsc::Receiver<CapturedSend>, what: &str) -> CapturedSend {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Ok(got) = rx.try_recv() {
            return got;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no /send within 15 s: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Sends a chat message, retrying while the channel join is still in flight
/// (the send reports NotConnected until Nakama's join reply arrives).
async fn send_when_joined(client: &NakamaClient, text: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match client.send_chat_message(text).await {
            Ok(()) => return,
            Err(e) if std::time::Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => panic!("send {text:?}: {e}"),
        }
    }
}

async fn joined_member(code: &str, token: &str) -> (NakamaClient, String) {
    let mut member = NakamaClient::new(e2e_config());
    member
        .authenticate_device(&random_device_id())
        .await
        .expect("device auth for the member");
    member
        .join_by_invite_code(code)
        .await
        .expect("join by invite");
    let id = member.current_user_id().expect("member id").to_string();
    member
        .register_push_token(token, "ios", "sandbox")
        .await
        .expect("register");
    (member, id)
}

/// Spec 23 M4 + M5, against a live Nakama and a stub Worker:
/// 1. A mention of an offline member is posted to the Worker at once, with the
///    tap route and the mention shown as a name.
/// 2. A mention of a member whose desktop is open but inactive is held for the
///    grace period (`PUSH_DESKTOP_GRACE_SECS`, 2 s under `scripts/e2e.sh`).
///
/// One test, because both scenarios share the stub's fixed port.
#[test]
fn mention_pushes_follow_the_delivery_rules() {
    if !e2e_enabled("mention_pushes_follow_the_delivery_rules") {
        return;
    }
    let (Some(port), Some(grace)) = (
        std::env::var("MELLO_E2E_PUSH_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok()),
        std::env::var("PUSH_DESKTOP_GRACE_SECS")
            .ok()
            .and_then(|g| g.parse::<u64>().ok()),
    ) else {
        println!("SKIP mention_pushes_follow_the_delivery_rules: run ./scripts/e2e.sh (push stub env unset)");
        return;
    };
    let grace = std::time::Duration::from_secs(grace);
    let captured = stub_push_worker(port);

    rt().block_on(async {
        let (mut owner, crew_id, code) = crew_with_invite(true).await;
        let (event_tx, _events) = std::sync::mpsc::channel();
        owner.connect_ws(event_tx).await.expect("owner socket");
        owner.join_crew_channel(&crew_id).await.expect("join crew channel");

        // 1. Offline member: pushed at once.
        let (offline, offline_id) = joined_member(&code, "feedface01").await;
        send_when_joined(&owner, &format!("gg <@{offline_id}> ping")).await;
        let got = next_send(&captured, "mention of an offline member").await;
        let head = got.head.to_ascii_lowercase();
        assert!(head.starts_with("post /send "), "{}", got.head);
        assert!(head.contains("authorization: bearer e2e-push-token"), "{}", got.head);
        let n = &got.body["notification"];
        assert_eq!(n["type"], "mention");
        assert_eq!(n["crew_id"], crew_id.as_str());
        assert!(!n["message_id"].as_str().unwrap_or_default().is_empty());
        assert!(!n["channel_id"].as_str().unwrap_or_default().is_empty());
        let text = n["body"].as_str().unwrap_or_default();
        assert!(text.contains("ping") && !text.contains("<@"), "mention must show as a name: {text}");
        assert_eq!(
            got.body["tokens"],
            serde_json::json!([{"token": "feedface01", "platform": "ios", "environment": "sandbox"}])
        );

        // 2. Desktop open but inactive: held for the grace period.
        let (mut away, away_id) = joined_member(&code, "feedface02").await;
        let (away_tx, _away_events) = std::sync::mpsc::channel();
        away.connect_ws(away_tx).await.expect("member socket");
        away.set_session_activity(false, "desktop").await.expect("report inactive");
        // Nakama handles one session's messages in order, so once this send
        // succeeds the activity report above has been processed.
        away.join_crew_channel(&crew_id).await.expect("member joins channel");
        send_when_joined(&away, "brb").await;

        let sent_at = std::time::Instant::now();
        owner
            .send_chat_message(&format!("<@{away_id}> you there?"))
            .await
            .expect("mention the away member");
        let got = next_send(&captured, "held mention of an inactive desktop user").await;
        assert_eq!(got.body["tokens"][0]["token"], "feedface02");
        let waited = got.at.duration_since(sent_at);
        assert!(
            waited >= grace.saturating_sub(std::time::Duration::from_millis(300)),
            "the push must wait for the grace period ({grace:?}), came after {waited:?}"
        );

        for c in [offline, away, owner] {
            let _ = c.delete_account().await;
        }
    });
}
