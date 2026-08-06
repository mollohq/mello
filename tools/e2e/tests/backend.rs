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
