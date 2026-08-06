//! Guards the *first* link in the discovery-failure chain.
//!
//! The client-side flow tests assert what the UI does when
//! `Event::DiscoverCrewsFailed` arrives. They cannot tell you whether core
//! actually emits it — and for a long time it did not: the error arm only
//! logged, leaving `onboarding_step` at 0, which renders neither the
//! onboarding branch nor the app branch. A brand new user saw an empty window
//! and closed it, and no server-side error was recorded because the request
//! that failed was the very first one.
//!
//! Needs no server: pointing the client at a closed port produces a real
//! connection error through the real code path.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use mello_core::{Client, Command, Config, Event};

/// A port nothing is listening on, so the HTTP call fails fast with a
/// connection error rather than hanging or reaching a real service.
fn config_pointing_at_nothing() -> Config {
    let mut cfg = Config::development();
    cfg.nakama_host = "127.0.0.1".into();
    // Chosen from the ephemeral range and never bound by this test.
    cfg.nakama_port = 1;
    cfg.nakama_ssl = false;
    cfg
}

#[test]
fn failed_discovery_emits_an_event_rather_than_only_logging() {
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let mut client = Client::new_with_game_sensor(
        config_pointing_at_nothing(),
        event_tx,
        false,
        Arc::new(Mutex::new(None)),
        Default::default(),
        Arc::new(AtomicBool::new(true)),
        Arc::new(std::sync::atomic::AtomicU8::new(0)),
        // No game sensing or process stats: this test is about one HTTP call,
        // and scanning processes would just add noise and startup cost.
        false,
        false,
    );

    cmd_tx
        .send(Command::DiscoverCrews { cursor: None })
        .expect("queue the command");
    drop(cmd_tx); // so `run` returns once the queue drains

    rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(20), client.run(cmd_rx))
            .await
            .expect("client.run should finish once the command channel closes");
    });

    let events: Vec<Event> = event_rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::DiscoverCrewsFailed { .. })),
        "a failed discovery must emit DiscoverCrewsFailed so the UI can show an \
         error and a retry. Without it the user is left on onboarding step 0, \
         which renders nothing at all. Events seen: {events:?}"
    );
}
