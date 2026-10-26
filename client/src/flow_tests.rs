//! End-to-end tests over whole user journeys, driven through the headless
//! [`crate::testkit::Harness`].
//!
//! These exercise the real `callbacks::wire_all` / `handlers::handle_event`
//! wiring, so they fail when a change alters what the UI asks core to do or how
//! it reacts to core events.

use std::ops::ControlFlow;

use base64::Engine as _;
use i_slint_backend_testing::ElementHandle;
use mello_core::crew_events::{FeedEntry, FeedResponse, FeedSection};
use mello_core::{decode_clip_waveform, Command, Event, VoiceMode};
use slint::{ComponentHandle as _, Model};

use crate::testkit::{Harness, MainWindow};
use crate::FeedCardData;

/// Which top-level screen the window is showing.
///
/// `main.slint` gates exactly three mutually exclusive branches:
/// - `Onboarding`  when `1 <= step <= 3 && !show-sign-in`
/// - `SignInPanel` when `show-sign-in && !logged-in`
/// - the app       when `logged-in && (step == 0 || step > 3)`
///
/// Nothing forces one of them to match, which is how the app can end up
/// showing an empty window.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Screen {
    Onboarding,
    SignIn,
    App,
    /// No top-level screen matched: the user sees an empty window.
    Blank,
}

fn visible_screens(h: &Harness) -> Vec<Screen> {
    let app = h.app();
    let present = |type_name: &str| {
        ElementHandle::find_by_element_type_name(app, type_name)
            .next()
            .is_some()
    };

    let mut found = Vec::new();
    if present("Onboarding") {
        found.push(Screen::Onboarding);
    }
    if present("SignInPanel") {
        found.push(Screen::SignIn);
    }
    // The app branch is an anonymous Rectangle; CrewPanel is its first child
    // and appears nowhere else.
    if present("CrewPanel") {
        found.push(Screen::App);
    }
    if found.is_empty() {
        found.push(Screen::Blank);
    }
    found
}

fn screen_for(h: &Harness, step: i32, logged_in: bool, show_sign_in: bool) -> Vec<Screen> {
    h.app().set_onboarding_step(step);
    h.app().set_logged_in(logged_in);
    h.app().set_show_sign_in(show_sign_in);
    visible_screens(h)
}

/// Every combination of the three properties that gate the top-level screens.
/// Step 5 is the invite welcome screen; 6 stands for any unknown step.
fn all_states() -> impl Iterator<Item = (i32, bool, bool)> {
    (0..=6).flat_map(|step| {
        [false, true].into_iter().flat_map(move |logged_in| {
            [false, true]
                .into_iter()
                .map(move |show_sign_in| (step, logged_in, show_sign_in))
        })
    })
}

/// Two screens must never render at once.
#[test]
fn at_most_one_screen_is_ever_visible() {
    let h = Harness::new();

    for (step, logged_in, show_sign_in) in all_states() {
        let screens = screen_for(&h, step, logged_in, show_sign_in);
        assert!(
            screens.len() == 1,
            "step={step} logged_in={logged_in} show_sign_in={show_sign_in} \
             rendered overlapping screens: {screens:?}"
        );
    }
}

/// Characterisation test for states that render **nothing at all**.
///
/// This is the class of bug behind the signup outage: when `discover_crews`
/// failed, `onboarding_step` stayed at 0 while `logged_in` was false, matching
/// no branch. The user got an empty window — no error, no retry, no way
/// forward — and nothing in the codebase objected.
///
/// The expected set is written out explicitly rather than asserted away,
/// because these states are reachable and currently unhandled. If a change
/// *adds* a dead state this test fails; if a change *fixes* one it also fails,
/// and the list should be narrowed deliberately.
#[test]
fn dead_end_states_are_exactly_the_known_set() {
    let h = Harness::new();

    // (step, logged_in, show_sign_in)
    let expected_dead: &[(i32, bool, bool)] = &[
        // Logged out, not in an onboarding step, no sign-in panel. Reached when
        // discover_crews fails at startup: its error arm logs and emits no
        // event, so nothing ever moves the step off 0.
        (0, false, false),
        // Onboarding finished (step 4, or an unknown step) but not logged
        // in. Reached if the persisted step survives while the session does
        // not.
        (4, false, false),
        (6, false, false),
        // Mid-onboarding, logged in, sign-in requested: onboarding is
        // suppressed by show-sign-in, sign-in by logged-in, and the app by the
        // step range. Step 5 is the invite welcome screen.
        (1, true, true),
        (2, true, true),
        (3, true, true),
        (5, true, true),
    ];

    let mut actual_dead: Vec<(i32, bool, bool)> = all_states()
        .filter(|&(step, logged_in, show_sign_in)| {
            screen_for(&h, step, logged_in, show_sign_in) == vec![Screen::Blank]
        })
        .collect();
    actual_dead.sort();

    let mut expected_dead = expected_dead.to_vec();
    expected_dead.sort();

    assert_eq!(
        actual_dead, expected_dead,
        "the set of blank-window states changed.\n\
         If you fixed one, remove it from expected_dead.\n\
         If you added one, that is a regression: a user in that state sees an \
         empty window with no way forward."
    );
}

/// The startup state specifically — the one a brand-new user lands in.
#[test]
fn fresh_install_startup_state_is_blank_until_crews_load() {
    let h = Harness::new();

    assert_eq!(h.app().get_onboarding_step(), 0);
    assert!(!h.app().get_logged_in());
    assert_eq!(
        visible_screens(&h),
        vec![Screen::Blank],
        "a fresh install shows nothing until DiscoverCrewsLoaded arrives"
    );
}

/// ★ Regression: a successful discover moves the user onto step 1.
#[test]
fn discover_crews_loaded_advances_to_step_one() {
    let mut h = Harness::new();

    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });

    assert_eq!(h.app().get_onboarding_step(), 1);
    assert_eq!(visible_screens(&h), vec![Screen::Onboarding]);
    h.assert_not_blank();
}

/// ★ Regression: zero discoverable crews must still leave a way forward.
///
/// `bento_bases(0, 5)` returns an empty vec, and the "Create Your Own Crew"
/// card lives inside `for base in bento-set-bases`, so with no crews the loop
/// body never instantiates and step 1 offers nothing at all.
#[test]
fn onboarding_with_zero_crews_still_offers_a_way_forward() {
    let mut h = Harness::new();

    h.emit(Event::DiscoverCrewsLoaded {
        crews: Vec::new(),
        cursor: None,
    });

    assert_eq!(
        h.app().get_onboarding_step(),
        1,
        "an empty crew list should still advance onboarding"
    );
    h.assert_not_blank();

    // With no crews to join, creating one is the *only* way forward, so the
    // Create Crew card must be present.
    //
    // Asserted structurally rather than via accessible_enabled(): almost
    // nothing in these panels declares an accessibility role yet, so an
    // a11y-based check reports zero enabled controls even on a healthy screen
    // and would fail for the wrong reason.
    let create_cards = ElementHandle::find_by_element_type_name(h.app(), "CreateCrewCard").count();
    let crew_cards = ElementHandle::find_by_element_type_name(h.app(), "CrewCard").count();

    assert_eq!(crew_cards, 0, "there are no crews to show");
    assert!(
        create_cards > 0,
        "step 1 with zero discoverable crews offers no Create Crew card, so the \
         user cannot join a crew, cannot create one, and cannot proceed — this \
         is the dead end that blocked signup"
    );
}

/// ★ Regression: a failed discovery must not leave the user staring at nothing.
///
/// This is the exact shape of the signup outage. `handle_discover_crews` used
/// to log its error and emit no event, so `onboarding_step` stayed at 0 —
/// rendering neither the onboarding branch nor the app branch. The user opened
/// the app, saw an empty window, and closed it. Nothing on the server recorded
/// a failure, because the request that failed was the *first* one.
#[test]
fn discover_failure_shows_an_error_and_a_way_forward() {
    let mut h = Harness::new();

    h.emit(Event::DiscoverCrewsFailed {
        reason: "HTTP 401 Unauthorized".into(),
    });

    // 1. Not blank.
    h.assert_not_blank();
    assert_eq!(visible_screens(&h), vec![Screen::Onboarding]);

    // 2. The failure is visible rather than log-only.
    assert_eq!(
        h.app().get_discover_error().as_str(),
        "HTTP 401 Unauthorized",
        "the failure reason must reach the UI"
    );

    // 3. There is still a way in, even if discovery never recovers.
    let create_cards = ElementHandle::find_by_element_type_name(h.app(), "CreateCrewCard").count();
    assert!(
        create_cards > 0,
        "with discovery broken, creating a crew is the only route in and must \
         still be offered"
    );
}

/// The Retry button must actually re-issue the request and clear the error.
#[test]
fn retry_after_discover_failure_reissues_the_request() {
    let mut h = Harness::new();
    h.emit(Event::DiscoverCrewsFailed {
        reason: "connection refused".into(),
    });
    let _ = h.commands();

    h.app().invoke_retry_discover();

    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::DiscoverCrews { cursor: None })),
        "Retry should re-issue DiscoverCrews, got {cmds:?}"
    );
    assert_eq!(
        h.app().get_discover_error().as_str(),
        "",
        "the error must clear while the retry is in flight"
    );
}

/// A later success must clear a previously shown error.
#[test]
fn successful_discover_clears_a_previous_error() {
    let mut h = Harness::new();
    h.emit(Event::DiscoverCrewsFailed {
        reason: "timeout".into(),
    });
    assert_ne!(h.app().get_discover_error().as_str(), "");

    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(2),
        cursor: None,
    });

    assert_eq!(
        h.app().get_discover_error().as_str(),
        "",
        "a successful load must clear the stale error banner"
    );
}

/// The normal case must keep working: crews render, and the Create Crew card
/// still appears exactly once alongside them.
#[test]
fn onboarding_with_crews_shows_cards_and_one_create_card() {
    let mut h = Harness::new();

    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });

    let create_cards = ElementHandle::find_by_element_type_name(h.app(), "CreateCrewCard").count();
    let crew_cards = ElementHandle::find_by_element_type_name(h.app(), "CrewCard").count();

    assert_eq!(crew_cards, 3, "one card per discoverable crew");
    assert_eq!(create_cards, 1, "exactly one Create Crew card");
}

fn sample_crews(n: usize) -> Vec<mello_core::crew::Crew> {
    (0..n)
        .map(|i| mello_core::crew::Crew {
            id: format!("crew-{i}"),
            name: format!("Crew {i}"),
            description: format!("A crew for testing, number {i}"),
            member_count: 3,
            max_members: 10,
            open: true,
            avatar_url: None,
        })
        .collect()
}

/// Drive the step-3 "continue" that fires `FinalizeOnboarding`.
///
/// The nickname is a Slint `out` property so Rust cannot set it; it is not
/// what these tests are about.
fn finalize(h: &Harness) {
    h.app().invoke_onboarding_continue(3);
}

fn finalize_device_ids(cmds: &[Command]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::FinalizeOnboarding { device_id, .. } => Some(device_id.clone()),
            _ => None,
        })
        .collect()
}

/// ★ Regression: every finalize attempt must reuse one device identity.
///
/// Onboarding used to mint a fresh random device id inside each attempt, and
/// never persisted it. Since device auth runs with `create=true`, a retry — or
/// a restart before onboarding was marked complete — authenticated as a new
/// device and Nakama created a *new account*. Production users ended up with
/// five or six each, and the second attempt then failed with "group name in
/// use" because the first had already created their crew.
#[test]
fn retrying_finalize_reuses_the_same_device_id() {
    let mut h = Harness::new();

    finalize(&h);
    let first = finalize_device_ids(&h.commands());
    assert_eq!(first.len(), 1, "one attempt should emit one finalize");
    assert!(!first[0].is_empty(), "device id must not be empty");

    h.emit(Event::OnboardingFailed {
        reason: "Connection failed: timed out".into(),
        join_error: None,
    });

    finalize(&h);
    let second = finalize_device_ids(&h.commands());
    assert_eq!(second.len(), 1);
    assert_eq!(
        first[0], second[0],
        "the retry must reuse the device id; a fresh one authenticates as a new \
         device and Nakama creates a second account for the same person"
    );
}

/// ★ Regression: a second click while finalize is in flight must be ignored.
///
/// The step-2 Continue button fires seven sequential network calls with no
/// visible progress. Users clicked it repeatedly — one production user six
/// times in 22 seconds — and each click started another signup.
#[test]
fn clicking_continue_twice_only_finalizes_once() {
    let mut h = Harness::new();

    finalize(&h);
    finalize(&h);

    let ids = finalize_device_ids(&h.commands());
    assert_eq!(
        ids.len(),
        1,
        "a second click while the first is in flight must be dropped, got {ids:?}"
    );
}

/// ...and once the attempt resolves, the user must be able to try again.
#[test]
fn the_finalize_guard_releases_after_a_failure() {
    let mut h = Harness::new();

    finalize(&h);
    assert_eq!(finalize_device_ids(&h.commands()).len(), 1);

    h.emit(Event::OnboardingFailed {
        reason: "Failed to create crew".into(),
        join_error: None,
    });

    finalize(&h);
    assert_eq!(
        finalize_device_ids(&h.commands()).len(),
        1,
        "after a failure the user is still on the same step and must be able to retry"
    );
}

fn finalize_avatar(cmds: &[Command]) -> Option<Option<String>> {
    cmds.iter().find_map(|c| match c {
        Command::FinalizeOnboarding { crew_avatar, .. } => Some(crew_avatar.clone()),
        _ => None,
    })
}

/// ★ Regression: retrying after a failed finalize must still carry the avatar.
///
/// `FinalizeOnboarding` runs seven sequential network calls and any of them can
/// fail, leaving the user on step 3 to try again. The pending crew avatar used
/// to be `.take()`n when the command was built, so the retry silently sent
/// none — a user who hit one transient error lost the avatar they had picked,
/// with nothing to indicate why.
#[test]
fn retrying_finalize_after_failure_preserves_the_crew_avatar() {
    let mut h = Harness::new();
    *h.ctx().new_crew_avatar_b64.lock().unwrap() = Some("BASE64_AVATAR".into());

    finalize(&h);
    let first = finalize_avatar(&h.commands());
    assert_eq!(
        first,
        Some(Some("BASE64_AVATAR".to_string())),
        "the first attempt should carry the avatar"
    );

    // Any of the seven steps failing lands here.
    h.emit(Event::OnboardingFailed {
        reason: "Connection failed: timed out".into(),
        join_error: None,
    });

    finalize(&h);
    let second = finalize_avatar(&h.commands());
    assert_eq!(
        second,
        Some(Some("BASE64_AVATAR".to_string())),
        "the retry must still carry the avatar the user picked; losing it here \
         is silent data loss they cannot diagnose"
    );
}

/// ...but a *successful* onboarding must release it, so the next crew the user
/// creates does not inherit the previous avatar.
#[test]
fn successful_onboarding_clears_the_pending_crew_avatar() {
    let mut h = Harness::new();
    *h.ctx().new_crew_avatar_b64.lock().unwrap() = Some("BASE64_AVATAR".into());

    h.emit(Event::OnboardingReady {
        user: sample_user(),
    });

    assert!(
        h.ctx().new_crew_avatar_b64.lock().unwrap().is_none(),
        "a completed onboarding must release the pending avatar, or the next \
         crew created would silently reuse it"
    );
}

fn sample_user() -> mello_core::events::User {
    mello_core::events::User {
        id: "user-1".into(),
        username: "tester".into(),
        display_name: "Test User".into(),
        tag: "#0001".into(),
        created_at: None,
    }
}

// ---------------------------------------------------------------------------
// Auth / session
// ---------------------------------------------------------------------------

/// `OnboardingReady` lands the user on step **3**, not 4 — reaching "done"
/// needs a separate later event (`EmailLinked` / `SocialLinked` / `LoggedIn`).
/// Step 3 has no skip. Pinned because it is surprising: an
/// account exists and `logged-in` is true while onboarding is still on screen.
#[test]
fn onboarding_ready_logs_in_but_stays_on_step_three() {
    let mut h = Harness::new();

    h.emit(Event::OnboardingReady {
        user: sample_user(),
    });

    assert!(h.app().get_logged_in(), "the account exists at this point");
    assert_eq!(
        h.app().get_onboarding_step(),
        3,
        "OnboardingReady deliberately stops at step 3, not 4"
    );
    assert_eq!(h.app().get_user_name().as_str(), "Test User");
    assert_eq!(visible_screens(&h), vec![Screen::Onboarding]);

    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::LoadMyCrews)),
        "expected LoadMyCrews after onboarding completes, got {cmds:?}"
    );
}

/// A successful login must land on a usable app screen, not a dead state.
#[test]
fn login_success_shows_the_app() {
    let mut h = Harness::new();

    h.emit(Event::LoggedIn {
        user: sample_user(),
    });

    assert!(h.app().get_logged_in());
    assert!(
        h.app().get_onboarding_step() > 3,
        "a logged-in user must be past onboarding, else the app branch cannot match"
    );
    assert_eq!(visible_screens(&h), vec![Screen::App]);
    h.assert_not_blank();
}

/// Reason-string-driven control flow: an **empty** reason means "session
/// restore failed" and silently drops the user back to step 1, while a
/// non-empty reason is a real login error that must surface to the user.
///
/// Pinned because the two paths are distinguished only by an empty string —
/// a refactor that fills in a default message would silently disable the
/// restore fallback.
#[test]
fn empty_login_failure_reason_means_restore_failed() {
    let mut h = Harness::new();
    h.app().set_onboarding_step(4);

    h.emit(Event::LoginFailed {
        reason: String::new(),
    });

    assert_eq!(
        h.app().get_onboarding_step(),
        1,
        "an empty reason is the restore-failed path and returns to onboarding"
    );
    assert!(!h.app().get_logged_in());
}

#[test]
fn real_login_failure_surfaces_an_error_and_stays_put() {
    let mut h = Harness::new();
    h.app().set_onboarding_step(4);

    h.emit(Event::LoginFailed {
        reason: "Authentication failed: Invalid credentials.".into(),
    });

    assert_eq!(
        h.app().get_login_error().as_str(),
        "Wrong email or password.",
        "a real failure must be shown to the user, in plain words"
    );
    assert_eq!(
        h.app().get_onboarding_step(),
        4,
        "a real failure must not silently restart onboarding"
    );
    assert!(!h.app().get_login_loading(), "the spinner must be cleared");
}

// ---------------------------------------------------------------------------
// Voice
// ---------------------------------------------------------------------------

/// Deafening implies muting, and undeafening restores the *previous* mic state
/// rather than blindly unmuting.
#[test]
fn deafen_mutes_and_undeafen_restores_previous_mic_state() {
    let mut h = Harness::new();

    // Deafen while unmuted: mic must be muted as a side effect.
    h.app().invoke_deafen_toggle();
    assert!(h.app().get_deafened());
    assert!(h.app().get_mic_muted(), "deafening should mute the mic");

    // Undeafen: mic returns to its pre-deafen state (unmuted).
    h.app().invoke_deafen_toggle();
    assert!(!h.app().get_deafened());
    assert!(
        !h.app().get_mic_muted(),
        "undeafening should restore the mic to its pre-deafen state"
    );

    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::SetDeafen { .. })),
        "expected SetDeafen commands, got {cmds:?}"
    );
}

/// Deafening while *already muted* must leave the mic muted afterwards.
#[test]
fn undeafen_keeps_mic_muted_when_it_was_muted_before() {
    let h = Harness::new();

    h.app().invoke_mic_toggle();
    assert!(h.app().get_mic_muted());

    h.app().invoke_deafen_toggle();
    h.app().invoke_deafen_toggle();

    assert!(
        h.app().get_mic_muted(),
        "the user muted deliberately; undeafening must not unmute them"
    );
}

/// core → UI: voice state drives the in-call indicator.
#[test]
fn voice_state_change_updates_the_ui() {
    let mut h = Harness::new();

    h.emit(Event::VoiceStateChanged {
        in_call: true,
        transport: VoiceMode::SFU,
    });
    assert!(h.app().get_in_voice());

    h.emit(Event::VoiceStateChanged {
        in_call: false,
        transport: VoiceMode::Disconnected,
    });
    assert!(!h.app().get_in_voice());
}

/// A crew state with a default channel `General` and a channel `Squad`.
/// `in_squad` lists the user ids in `Squad`.
fn crew_state_with_channels(in_squad: &[&str]) -> mello_core::crew_state::CrewState {
    use mello_core::crew_state::{CrewState, VoiceChannelState, VoiceMember};
    CrewState {
        crew_id: "crew-1".into(),
        name: "Crew".into(),
        voice_channels: vec![
            VoiceChannelState {
                id: "ch-general".into(),
                name: "General".into(),
                is_default: true,
                members: vec![],
                ..Default::default()
            },
            VoiceChannelState {
                id: "ch-squad".into(),
                name: "Squad".into(),
                is_default: false,
                bitrate_kbps: 96,
                members: in_squad
                    .iter()
                    .map(|id| VoiceMember {
                        user_id: (*id).into(),
                        username: (*id).into(),
                        ..Default::default()
                    })
                    .collect(),
            },
        ],
        ..Default::default()
    }
}

/// The ids of the voice channels the UI marks active.
fn active_voice_channels(h: &Harness) -> Vec<String> {
    let channels = h.app().get_voice_channels();
    (0..channels.row_count())
        .filter_map(|i| channels.row_data(i))
        .filter(|ch| ch.active)
        .map(|ch| ch.id.to_string())
        .collect()
}

/// Join `Squad` in a crew with two channels. The call has not started yet.
fn join_squad_before_the_call_starts(h: &mut Harness) {
    h.app().set_user_id("u-me".into());
    h.app().set_active_crew_id("crew-1".into());
    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&[]),
    });
    assert!(active_voice_channels(h).is_empty(), "no channel is active");

    h.emit(Event::VoiceJoined {
        crew_id: "crew-1".into(),
        channel_id: "ch-squad".into(),
        members: vec![mello_core::crew_state::VoiceMember {
            user_id: "u-me".into(),
            username: "me".into(),
            ..Default::default()
        }],
    });
    assert_eq!(active_voice_channels(h), vec!["ch-squad"]);
}

/// ★ Regression (#129): an SFU call starts about 0.8 s after `VoiceJoined`.
/// A crew update before that rebuilt the channels and dropped the active
/// mark, because the rebuild kept it only when `in_voice` was true.
#[test]
fn a_crew_update_before_the_call_starts_keeps_the_joined_channel_active() {
    let mut h = Harness::new();
    join_squad_before_the_call_starts(&mut h);

    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&["u-me"]),
    });
    assert_eq!(
        active_voice_channels(&h),
        vec!["ch-squad"],
        "the crew update came before the call started"
    );

    h.emit(Event::VoiceStateChanged {
        in_call: true,
        transport: VoiceMode::SFU,
    });
    assert_eq!(active_voice_channels(&h), vec!["ch-squad"]);

    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&["u-me"]),
    });
    assert_eq!(active_voice_channels(&h), vec!["ch-squad"]);
}

/// The texts on screen now.
fn visible_texts(h: &Harness) -> Vec<String> {
    h.elements()
        .iter()
        .filter_map(|e| e.accessible_label())
        .map(|s| s.to_string())
        .collect()
}

/// Spec 10 section 7.6: a dropped SFU voice link shows "reconnecting" on the
/// joined channel's header and on the local member's row, from
/// `VoiceSfuDisconnected` until the call is back or the reconnect gives up.
#[test]
fn a_dropped_sfu_link_shows_reconnecting_until_the_call_is_back() {
    use crate::CrewData;

    let mut h = Harness::new();
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.app()
        .set_crews(slint::ModelRc::new(slint::VecModel::from(vec![CrewData {
            id: "crew-1".into(),
            name: "Crew".into(),
            ..Default::default()
        }])));
    join_squad_before_the_call_starts(&mut h);
    h.emit(Event::VoiceStateChanged {
        in_call: true,
        transport: VoiceMode::SFU,
    });
    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&["u-me", "u-bob"]),
    });
    let shows = |h: &Harness, text: &str| visible_texts(h).iter().any(|t| t == text);
    assert!(!h.app().get_voice_reconnecting());
    assert!(!shows(&h, "RECONNECTING"));

    h.emit(Event::VoiceSfuDisconnected {
        crew_id: "crew-1".into(),
        reason: "ws_closed".into(),
    });
    assert!(h.app().get_voice_reconnecting());
    assert!(
        shows(&h, "RECONNECTING"),
        "the channel header: {:?}",
        visible_texts(&h)
    );
    // The test backend's tree walk meets each row more than once, so
    // compare counts: the line shows as often as the local member's name,
    // and never with the other member's.
    let count = |h: &Harness, text: &str| {
        visible_texts(h)
            .iter()
            .filter(|t| t.as_str() == text)
            .count()
    };
    assert!(count(&h, "Reconnecting") > 0, "the local member's row");
    assert_eq!(
        count(&h, "Reconnecting"),
        count(&h, "u-me"),
        "only the local member's row says so"
    );
    assert_eq!(
        active_voice_channels(&h),
        vec!["ch-squad"],
        "the channel stays joined while it reconnects"
    );

    // The reconnect succeeds.
    h.emit(Event::VoiceStateChanged {
        in_call: true,
        transport: VoiceMode::SFU,
    });
    assert!(!h.app().get_voice_reconnecting());
    assert!(!shows(&h, "RECONNECTING"));
    assert!(!shows(&h, "Reconnecting"));

    // The reconnect gives up.
    h.emit(Event::VoiceSfuDisconnected {
        crew_id: "crew-1".into(),
        reason: "liveness_timeout".into(),
    });
    assert!(h.app().get_voice_reconnecting());
    h.emit(Event::VoiceStateChanged {
        in_call: false,
        transport: VoiceMode::Disconnected,
    });
    assert!(!h.app().get_voice_reconnecting());
    assert!(!shows(&h, "RECONNECTING"));
}

/// A silent SFU link (no pong for 1 s, a pulled cable) shows the same
/// "reconnecting" state while the session waits for the link, and clears
/// when pongs come back.
#[test]
fn a_stalled_sfu_link_shows_reconnecting_until_pongs_return() {
    use crate::CrewData;

    let mut h = Harness::new();
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.app()
        .set_crews(slint::ModelRc::new(slint::VecModel::from(vec![CrewData {
            id: "crew-1".into(),
            name: "Crew".into(),
            ..Default::default()
        }])));
    join_squad_before_the_call_starts(&mut h);
    h.emit(Event::VoiceStateChanged {
        in_call: true,
        transport: VoiceMode::SFU,
    });
    let shows = |h: &Harness| visible_texts(h).iter().any(|t| t == "RECONNECTING");
    assert!(!shows(&h));

    h.emit(Event::VoiceLinkStalled {
        crew_id: "crew-1".into(),
        stalled: true,
    });
    assert!(shows(&h), "a stalled link shows on the channel header");
    h.emit(Event::VoiceLinkStalled {
        crew_id: "crew-1".into(),
        stalled: false,
    });
    assert!(!shows(&h), "the link came back");

    // A stall that ends in a drop and a reconnect clears with the new call.
    h.emit(Event::VoiceLinkStalled {
        crew_id: "crew-1".into(),
        stalled: true,
    });
    h.emit(Event::VoiceSfuDisconnected {
        crew_id: "crew-1".into(),
        reason: "liveness_timeout".into(),
    });
    h.emit(Event::VoiceStateChanged {
        in_call: true,
        transport: VoiceMode::SFU,
    });
    assert!(!shows(&h), "the new session starts unstalled");
}

/// The debug panel's network section names the preferred SFU region and its
/// round trip.
#[test]
fn the_debug_panel_shows_the_preferred_sfu_region() {
    let mut h = Harness::new();
    assert_eq!(h.app().get_dbg_sfu_region(), "");
    h.emit(Event::SfuRegionsMeasured {
        regions: vec![
            mello_core::transport::sfu_region::RegionRtt {
                region: "us-east".into(),
                rtt_ms: 31,
            },
            mello_core::transport::sfu_region::RegionRtt {
                region: "eu-west".into(),
                rtt_ms: 112,
            },
        ],
    });
    assert_eq!(h.app().get_dbg_sfu_region(), "us-east 31ms");
    h.emit(Event::SfuRegionsMeasured { regions: vec![] });
    assert_eq!(h.app().get_dbg_sfu_region(), "");
}

fn channel_bitrates(cmds: &[Command]) -> Vec<u32> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::SetVoiceChannelBitrate { kbps } => Some(*kbps),
            _ => None,
        })
        .collect()
}

/// Spec 10 section 4.6: the joined channel's bitrate reaches the encoder
/// when a channel update changes it. Other channels do not count, and
/// before a join nothing is sent.
#[test]
fn the_joined_channels_bitrate_goes_to_the_encoder_on_a_channel_update() {
    let mut h = Harness::new();
    h.app().set_user_id("u-me".into());
    h.app().set_active_crew_id("crew-1".into());
    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&[]),
    });
    assert!(
        channel_bitrates(&h.commands()).is_empty(),
        "no channel joined"
    );

    join_squad_before_the_call_starts(&mut h);
    h.commands();

    // Squad is at 96 kbps, General at the default.
    h.emit(Event::VoiceChannelsUpdated {
        crew_id: "crew-1".into(),
        channels: crew_state_with_channels(&["u-me"]).voice_channels,
    });
    assert_eq!(channel_bitrates(&h.commands()), vec![96]);

    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&["u-me"]),
    });
    assert_eq!(channel_bitrates(&h.commands()), vec![96]);

    // The local member changed the bitrate of another channel: nothing.
    h.emit(Event::VoiceChannelBitrateChanged {
        crew_id: "crew-1".into(),
        channel_id: "ch-general".into(),
        bitrate_kbps: 32,
    });
    assert!(channel_bitrates(&h.commands()).is_empty());
    h.emit(Event::VoiceChannelBitrateChanged {
        crew_id: "crew-1".into(),
        channel_id: "ch-squad".into(),
        bitrate_kbps: 48,
    });
    assert_eq!(channel_bitrates(&h.commands()), vec![48]);
}

/// Whether `member` shows the speaking ring in the `Squad` channel.
fn squad_member_speaking(h: &Harness, member: &str) -> bool {
    let channels = h.app().get_voice_channels();
    (0..channels.row_count())
        .filter_map(|i| channels.row_data(i))
        .filter(|ch| ch.id == "ch-squad")
        .flat_map(|ch| {
            (0..ch.members.row_count())
                .filter_map(|j| ch.members.row_data(j))
                .collect::<Vec<_>>()
        })
        .find(|m| m.id == member)
        .unwrap_or_else(|| panic!("no {member:?} in Squad"))
        .speaking
}

fn speaking_rpcs(h: &mut Harness) -> Vec<bool> {
    h.commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::VoiceSpeaking { speaking } => Some(speaking),
            _ => None,
        })
        .collect()
}

/// `voice_update` for Squad with `u-me` and `u-bob`, bob speaking per the
/// backend.
fn squad_update_with_bob_speaking(bob_speaking: bool) -> Event {
    use mello_core::crew_state::VoiceMember;
    Event::VoiceUpdated {
        crew_id: "crew-1".into(),
        channel_id: "ch-squad".into(),
        members: vec![
            VoiceMember {
                user_id: "u-me".into(),
                username: "me".into(),
                speaking: Some(false),
                ..Default::default()
            },
            VoiceMember {
                user_id: "u-bob".into(),
                username: "bob".into(),
                speaking: Some(bob_speaking),
                ..Default::default()
            },
        ],
    }
}

/// In Squad with bob over `transport`.
fn in_squad_with_bob(h: &mut Harness, transport: VoiceMode) {
    join_squad_before_the_call_starts(h);
    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&["u-me", "u-bob"]),
    });
    h.emit(Event::VoiceStateChanged {
        in_call: true,
        transport,
    });
    h.commands();
}

/// ★ Stage 4: in an SFU call a remote member's ring comes from the SFU's
/// in-band speaking message. It must not go back to the backend as this
/// user's speaking presence; only the local member's own state does.
#[test]
fn a_remote_speaking_ring_from_the_sfu_sends_no_presence_rpc() {
    let mut h = Harness::new();
    in_squad_with_bob(&mut h, VoiceMode::SFU);

    h.emit(Event::VoiceActivity {
        member_id: "u-bob".into(),
        speaking: true,
    });
    assert!(squad_member_speaking(&h, "u-bob"), "bob's ring shows");
    assert_eq!(speaking_rpcs(&mut h), Vec::<bool>::new());

    h.emit(Event::VoiceActivity {
        member_id: "u-me".into(),
        speaking: true,
    });
    assert!(squad_member_speaking(&h, "u-me"));
    assert_eq!(
        speaking_rpcs(&mut h),
        vec![true],
        "the local state is presence"
    );
}

/// ★ Stage 4: the backend's `voice_update` lags the SFU. In an SFU call a
/// stale `speaking: true` must not turn a ring back on that the SFU already
/// turned off.
#[test]
fn a_stale_backend_update_does_not_flicker_the_sfu_speaking_ring() {
    let mut h = Harness::new();
    in_squad_with_bob(&mut h, VoiceMode::SFU);

    h.emit(Event::VoiceActivity {
        member_id: "u-bob".into(),
        speaking: true,
    });
    h.emit(Event::VoiceActivity {
        member_id: "u-bob".into(),
        speaking: false,
    });
    h.emit(squad_update_with_bob_speaking(true));
    assert!(
        !squad_member_speaking(&h, "u-bob"),
        "the stale backend state must not override the SFU"
    );

    // And the other way: the SFU says bob talks, the backend says not yet.
    h.emit(Event::VoiceActivity {
        member_id: "u-bob".into(),
        speaking: true,
    });
    h.emit(squad_update_with_bob_speaking(false));
    assert!(squad_member_speaking(&h, "u-bob"));

    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&["u-me", "u-bob"]),
    });
    h.emit(Event::VoiceChannelsUpdated {
        crew_id: "crew-1".into(),
        channels: crew_state_with_channels(&["u-me", "u-bob"]).voice_channels,
    });
    assert!(
        squad_member_speaking(&h, "u-bob"),
        "a channel rebuild keeps it"
    );
}

/// ★ A join that fails after `VoiceJoined` ends with
/// `VoiceStateChanged { in_call: false }`. The active mark goes, and a later
/// crew update does not put it back on a channel the user is not in.
#[test]
fn a_failed_join_clears_the_active_channel() {
    let mut h = Harness::new();
    join_squad_before_the_call_starts(&mut h);

    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&["u-me"]),
    });
    h.emit(Event::VoiceStateChanged {
        in_call: false,
        transport: VoiceMode::Disconnected,
    });
    assert!(
        active_voice_channels(&h).is_empty(),
        "the join failed, so no channel is active"
    );

    h.emit(Event::CrewStateLoaded {
        state: crew_state_with_channels(&[]),
    });
    assert!(
        active_voice_channels(&h).is_empty(),
        "a crew update after the failed join marks no channel"
    );
}

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

/// The message the user typed must reach core intact.
#[test]
fn sending_a_message_carries_its_content() {
    let mut h = Harness::new();

    h.app().invoke_send_message("hello crew".into());

    let cmds = h.commands();
    let sent = cmds.iter().find_map(|c| match c {
        Command::SendMessage {
            content, reply_to, ..
        } => Some((content.clone(), reply_to.clone())),
        _ => None,
    });
    assert_eq!(
        sent,
        Some(("hello crew".to_string(), None)),
        "SendMessage must carry the typed text and no reply target, got {cmds:?}"
    );
}

/// Replies must keep both the body and the message being replied to; losing
/// either silently downgrades a reply into an ordinary message.
#[test]
fn replying_carries_both_body_and_parent() {
    let mut h = Harness::new();

    h.app()
        .invoke_send_message_with_reply("me too".into(), "msg-123".into());

    let cmds = h.commands();
    let sent = cmds.iter().find_map(|c| match c {
        Command::SendMessage {
            content, reply_to, ..
        } => Some((content.clone(), reply_to.clone())),
        _ => None,
    });
    assert_eq!(
        sent,
        Some(("me too".to_string(), Some("msg-123".to_string()))),
        "a reply must carry both body and parent id, got {cmds:?}"
    );
}

#[test]
fn editing_and_deleting_messages_target_the_right_id() {
    let mut h = Harness::new();

    h.app().invoke_edit_message("msg-1".into(), "fixed".into());
    h.app().invoke_delete_message("msg-2".into());

    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::EditMessage { message_id, new_body, .. }
                if message_id == "msg-1" && new_body == "fixed"
        )),
        "edit must target msg-1, got {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::DeleteMessage { message_id } if message_id == "msg-2")),
        "delete must target msg-2, got {cmds:?}"
    );
}

/// Picking a member shows `@name` in the composer (never the raw user id) and
/// sends the pick, so the core can write the `<@user_id>` token.
#[test]
fn picking_a_mention_shows_the_name_and_sends_the_pick() {
    let mut h = Harness::new();

    h.app().set_chat_input_text("gg @Al".into());
    h.app()
        .invoke_mention_selected("u-alice".into(), "Alice Baker".into());
    assert_eq!(h.app().get_chat_input_text(), "gg @Alice Baker ");

    h.app().invoke_send_message("gg @Alice Baker ".into());
    let cmds = h.commands();
    let mentions = cmds.iter().find_map(|c| match c {
        Command::SendMessage { mentions, .. } => Some(mentions.clone()),
        _ => None,
    });
    assert_eq!(
        mentions,
        Some(vec![mello_core::chat::MentionRef {
            user_id: "u-alice".into(),
            name: "Alice Baker".into(),
        }]),
        "SendMessage must carry the pick, got {cmds:?}"
    );

    // The pick is used once; the next message starts with no picks.
    h.app().invoke_send_message("plain text".into());
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::SendMessage { mentions, content, .. }
                if content == "plain text" && mentions.is_empty()
        )),
        "a later message must not reuse old picks, got {cmds:?}"
    );
}

/// The mention picker lists crew members whether or not they are online.
/// `MemberLeft` / `MemberJoined` are chat-channel presence (app closed or
/// backgrounded), not crew membership, so they must not drop anyone from the
/// picker. Bug: after mentioning a user who then backgrounded the app, a
/// second mention could not find them.
#[test]
fn the_mention_picker_keeps_members_who_went_offline() {
    let mut h = Harness::new();
    h.app().set_mention_members(
        std::rc::Rc::new(slint::VecModel::from(vec![
            crate::MentionMemberData {
                user_id: "u-alice".into(),
                display_name: "Alice Baker".into(),
                initials: "AB".into(),
            },
            crate::MentionMemberData {
                user_id: "u-kim".into(),
                display_name: "kim".into(),
                initials: "KI".into(),
            },
        ]))
        .into(),
    );
    let suggested = |h: &Harness| -> Vec<String> {
        h.app().set_chat_input_text("@".into());
        h.app().invoke_chat_input_changed("@".into());
        let m = h.app().get_mention_suggestions();
        (0..m.row_count())
            .filter_map(|i| m.row_data(i))
            .map(|r| r.display_name.to_string())
            .collect()
    };

    h.emit(Event::MemberLeft {
        crew_id: "crew-1".into(),
        member_id: "u-alice".into(),
    });
    assert_eq!(
        suggested(&h),
        ["Alice Baker", "kim"],
        "Alice only went offline"
    );

    h.emit(Event::MemberJoined {
        crew_id: "crew-1".into(),
        member: mello_core::crew::Member {
            id: "u-new".into(),
            username: "xYzRandom".into(),
            display_name: "Newcomer".into(),
            online: true,
        },
    });
    assert_eq!(
        suggested(&h),
        ["Alice Baker", "kim", "Newcomer"],
        "a join adds the newcomer and keeps the offline members"
    );
}

/// A mention renders as `@name` from the core's resolved body, also before the
/// crew member list has loaded, and flags the row when it mentions the user.
#[test]
fn a_mention_renders_as_a_name_before_the_member_list_loads() {
    let mut h = Harness::new();
    h.app().set_user_id("u-me".into());

    let (display_body, mentions) = mello_core::chat::resolve_mentions(
        "hey <@u-me>",
        &[("u-me".to_string(), "Me".to_string())]
            .into_iter()
            .collect(),
    );
    h.emit(Event::MessagesLoaded {
        messages: vec![mello_core::events::ChatMessage {
            message_id: "m1".into(),
            sender_id: "u-bob".into(),
            sender_name: "Bob".into(),
            content: "hey <@u-me>".into(),
            display_body,
            mentions,
            timestamp: "2026-03-08T12:00:00Z".into(),
            create_time: "2026-03-08T12:00:00Z".into(),
            update_time: "2026-03-08T12:00:00Z".into(),
            gif: None,
            reply_to: None,
            is_system: false,
            is_edited: false,
            is_deleted: false,
        }],
        has_more_history: false,
    });

    let rows = h.app().get_messages();
    let row = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .find(|r| r.message_id == "m1")
        .expect("the message row");
    assert_eq!(row.display_text, "hey @Me");
    assert_eq!(row.text, "hey @Me", "copy and edit must see the name");
    assert!(row.mentions_self);
}

// ---------------------------------------------------------------------------
// Crew selection and joining
// ---------------------------------------------------------------------------

#[test]
fn selecting_a_crew_sends_its_id() {
    let mut h = Harness::new();

    h.app().invoke_select_crew("crew-42".into());

    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::SelectCrew { crew_id } if crew_id == "crew-42")),
        "SelectCrew must carry the chosen crew id, got {cmds:?}"
    );
}

#[test]
fn joining_from_discover_uses_the_crew_id_and_invite_code_paths() {
    let mut h = Harness::new();

    h.app().invoke_discover_join_crew("crew-7".into());
    h.app().invoke_discover_join_invite("ABCD-1234".into());

    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::JoinCrew { crew_id } if crew_id == "crew-7")),
        "joining a listed crew must send JoinCrew, got {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::JoinByInviteCode { code } if code == "ABCD-1234")),
        "an invite code must go through JoinByInviteCode, got {cmds:?}"
    );
}

fn invite_person(name: &str) -> mello_core::crew::InvitePerson {
    mello_core::crew::InvitePerson {
        display_name: name.into(),
        avatar_seed: name.into(),
    }
}

fn sample_invite() -> mello_core::crew::ResolvedInvite {
    mello_core::crew::ResolvedInvite {
        crew_name: "Night Stones".into(),
        avatar_seed: "Night Stones".into(),
        crew_id: "crew-inv".into(),
        highlight: String::new(),
        member_count: 4,
        members: vec![invite_person("alice"), invite_person("bo")],
        inviter: Some(invite_person("alice")),
    }
}

fn join_crew_modal_is_visible(h: &Harness) -> bool {
    ElementHandle::find_by_element_type_name(h.app(), "JoinCrewModal")
        .next()
        .is_some()
}

/// The text of the join modal's error line, when it is on screen.
fn join_error_on_screen(h: &Harness) -> Option<String> {
    h.find("JoinCrewModal::join-error-text")
        .first()
        .and_then(|e| e.accessible_label())
        .map(|l| l.to_string())
}

/// A new user who opens `mello://join/<code>` joins the crew at the end of
/// onboarding. The join modal opens on top of onboarding step 3.
fn open_join_modal_during_onboarding(h: &mut Harness) {
    *h.ctx().pending_deep_link.borrow_mut() = Some(crate::deep_link::DeepLink::Join {
        code: "NITE-0001".into(),
    });
    h.emit(Event::OnboardingReady {
        user: sample_user(),
    });
    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::ResolveCrewInvite { code } if code == "NITE-0001")),
        "the pending link must be resolved once the account exists, got {cmds:?}"
    );

    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });
    assert!(join_crew_modal_is_visible(h));
    assert_eq!(visible_screens(h), vec![Screen::Onboarding]);
}

/// The join modal names the inviter and counts the members.
#[test]
fn the_join_modal_names_the_inviter_and_counts_the_members() {
    let mut h = Harness::new();
    h.app().set_join_crew_highlight("stale".into());
    open_join_modal_during_onboarding(&mut h);

    assert_eq!(
        text_on_screen(&h, "JoinCrewModal::inviter-text").as_deref(),
        Some("alice invited you")
    );
    assert_eq!(
        text_on_screen(&h, "JoinCrewModal::sub-line-text").as_deref(),
        Some("4 members"),
        "the member count; this invite has no highlight"
    );

    // An invite with no inviter has no inviter line.
    h.emit(Event::CrewInviteResolved {
        code: "NITE-0002".into(),
        invite: mello_core::crew::ResolvedInvite {
            inviter: None,
            highlight: "7h hangout".into(),
            ..sample_invite()
        },
    });
    assert_eq!(text_on_screen(&h, "JoinCrewModal::inviter-text"), None);
    assert_eq!(
        text_on_screen(&h, "JoinCrewModal::sub-line-text").as_deref(),
        Some("4 members · 7h hangout")
    );
}

/// ★ Regression: a failed invite join during onboarding was invisible.
///
/// Clicking "Join crew" closed the modal at once, and the failure went to the
/// generic error path, which only logs. The user was back on onboarding step 3
/// with no error. The error also said "Invalid invite code" for a server
/// failure.
#[test]
fn a_failed_invite_join_during_onboarding_is_visible() {
    let mut h = Harness::new();
    open_join_modal_during_onboarding(&mut h);

    h.click("JoinCrewModal::join-touch");
    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::JoinByInviteCode { code } if code == "NITE-0001")),
        "Join crew must send JoinByInviteCode, got {cmds:?}"
    );
    assert!(
        join_crew_modal_is_visible(&h),
        "the modal must stay open until the join succeeds or fails"
    );
    assert!(h.app().get_join_crew_joining());

    h.emit(Event::InviteJoinFailed {
        error: mello_core::crew::InviteError::Failed,
    });

    assert!(
        join_crew_modal_is_visible(&h),
        "the modal is the only surface above onboarding; it must stay open"
    );
    assert_eq!(visible_screens(&h), vec![Screen::Onboarding]);
    let shown = join_error_on_screen(&h).expect("the join error must be on screen");
    assert_eq!(shown, "Could not join the crew. Try again.");
    assert!(
        !shown.to_lowercase().contains("invalid"),
        "a server failure must not blame the invite code: {shown:?}"
    );
    assert!(
        !h.app().get_join_crew_joining(),
        "the user must be able to retry"
    );
    assert!(
        !h.find("JoinCrewModal::join-touch").is_empty(),
        "the Join crew button must stay available for a retry"
    );
}

/// A successful join closes the modal and clears any earlier error.
#[test]
fn a_successful_invite_join_closes_the_modal() {
    let mut h = Harness::new();
    open_join_modal_during_onboarding(&mut h);
    h.click("JoinCrewModal::join-touch");
    h.emit(Event::InviteJoinFailed {
        error: mello_core::crew::InviteError::CrewFull,
    });
    assert_eq!(
        join_error_on_screen(&h).as_deref(),
        Some("This crew is full.")
    );

    h.click("JoinCrewModal::join-touch");
    assert!(
        join_error_on_screen(&h).is_none(),
        "a retry clears the previous error"
    );
    h.emit(Event::InviteJoined {
        crew_id: "crew-inv".into(),
    });

    assert!(!join_crew_modal_is_visible(&h));
    assert!(!h.app().get_join_crew_joining());
}

/// A code typed into Discover has no modal. Its failure shows under the field.
#[test]
fn a_failed_join_from_the_discover_code_field_is_shown_there() {
    let mut h = Harness::new();
    h.app().invoke_discover_join_invite("NOPE-0000".into());

    h.emit(Event::InviteJoinFailed {
        error: mello_core::crew::InviteError::InvalidCode,
    });

    assert!(!join_crew_modal_is_visible(&h));
    assert_eq!(
        h.app().get_discover_invite_error().as_str(),
        "This invite code is not valid."
    );

    h.app().invoke_discover_join_invite("NITE-0001".into());
    assert_eq!(
        h.app().get_discover_invite_error().as_str(),
        "",
        "a new attempt clears the previous error"
    );
}

// ---------------------------------------------------------------------------
// An invite link on a fresh install (#68)
// ---------------------------------------------------------------------------

/// Start a fresh install from `mello://join/NITE-0001`, the way `lib.rs` does:
/// the startup dispatch, then resume at `Loading`.
fn start_fresh_install_from_invite(h: &mut Harness) -> Vec<Command> {
    *h.ctx().pending_deep_link.borrow_mut() = Some(crate::deep_link::DeepLink::Join {
        code: "NITE-0001".into(),
    });
    crate::onboarding_invite::dispatch_at_startup(
        h.ctx(),
        crate::onboarding::OnboardingState::Loading,
    );
    crate::onboarding::resume(h.ctx(), crate::onboarding::OnboardingState::Loading);
    h.commands()
}

/// The invite resolved: the fresh install is on the welcome screen.
fn welcome_on_fresh_install(h: &mut Harness, invite: mello_core::crew::ResolvedInvite) {
    start_fresh_install_from_invite(h);
    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite,
    });
}

/// The invite resolved and the user pressed "Join" on the welcome screen:
/// step 2, with the crew shown.
fn accept_invite_on_fresh_install(h: &mut Harness) {
    welcome_on_fresh_install(h, sample_invite());
    h.click_label("Join Night Stones");
    assert_eq!(h.app().get_onboarding_step(), 2);
}

fn welcome_is_visible(h: &Harness) -> bool {
    ElementHandle::find_by_element_type_name(h.app(), "InviteWelcome")
        .next()
        .is_some()
}

/// The step indicator text, for example "STEP 01 / 02".
fn step_indicator(h: &Harness) -> Option<String> {
    text_on_screen(h, "StepIndicator::step-text")
}

fn text_on_screen(h: &Harness, element_id: &str) -> Option<String> {
    h.find(element_id)
        .first()
        .and_then(|e| e.accessible_label())
        .map(|l| l.to_string())
}

fn finalize_invite(cmds: &[Command]) -> Option<(Option<String>, Option<String>, Option<String>)> {
    cmds.iter().find_map(|c| match c {
        Command::FinalizeOnboarding {
            invite_code,
            crew_id,
            crew_name,
            ..
        } => Some((invite_code.clone(), crew_id.clone(), crew_name.clone())),
        _ => None,
    })
}

/// ★ Regression (#68): a fresh install opened from an invite link skips
/// step 1, shows the invited crew on step 2, and finalize joins that crew by
/// its invite code.
///
/// Before the fix the link resolved only after the account existed. The user
/// made or joined another crew at step 1, and a join modal for the invited
/// crew opened on top of step 3.
#[test]
fn an_invite_link_on_a_fresh_install_skips_step_one() {
    let mut h = Harness::new();

    let cmds = start_fresh_install_from_invite(&mut h);
    let resolve = cmds
        .iter()
        .position(|c| matches!(c, Command::ResolveCrewInvite { code } if code == "NITE-0001"));
    let discover = cmds
        .iter()
        .position(|c| matches!(c, Command::DiscoverCrews { .. }));
    assert!(
        resolve.is_some(),
        "the invite must resolve before an account exists, got {cmds:?}"
    );
    assert!(
        resolve < discover,
        "the invite resolves before crew discovery, so step 1 does not show first: {cmds:?}"
    );

    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });
    assert!(
        welcome_is_visible(&h),
        "step 1 is skipped: the welcome screen"
    );
    h.click_label("Join Night Stones");
    assert_eq!(h.app().get_onboarding_step(), 2, "step 1 is skipped");
    assert!(
        !join_crew_modal_is_visible(&h),
        "no join modal: onboarding joins the crew"
    );
    assert_eq!(
        text_on_screen(&h, "Onboarding::invite-crew-text").as_deref(),
        Some("Night Stones"),
        "step 2 shows the crew that the user joins"
    );

    // A late discovery answer does not move the user back to step 1.
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });
    assert_eq!(h.app().get_onboarding_step(), 2);

    finalize(&h);
    assert_eq!(
        finalize_invite(&h.commands()),
        Some((Some("NITE-0001".into()), None, None)),
        "finalize joins by the invite code and neither joins nor creates another crew"
    );

    h.emit(Event::OnboardingReady {
        user: sample_user(),
    });
    assert_eq!(h.app().get_onboarding_step(), 3, "step 3 follows as usual");
    let after = h.commands();
    assert!(
        !after
            .iter()
            .any(|c| matches!(c, Command::ResolveCrewInvite { .. })),
        "the invite is used; it must not open the join modal on step 3: {after:?}"
    );
    assert!(h.settings().borrow().pending_invite_code.is_none());
}

/// ★ A fresh install opened from an invite shows the welcome screen: the
/// inviter, the crew and the members. Before, it went straight to step 2
/// with only a small "JOINING CREW" line, and testers did not know that
/// someone had invited them.
#[test]
fn a_fresh_install_from_an_invite_shows_the_welcome_screen() {
    let mut h = Harness::new();
    welcome_on_fresh_install(&mut h, sample_invite());

    assert_eq!(visible_screens(&h), vec![Screen::Onboarding]);
    assert!(welcome_is_visible(&h), "the welcome screen is on screen");
    assert_eq!(
        text_on_screen(&h, "InviteWelcome::inviter-text").as_deref(),
        Some("alice invited you to join")
    );
    assert_eq!(
        text_on_screen(&h, "InviteWelcome::crew-name-text").as_deref(),
        Some("Night Stones")
    );
    assert_eq!(
        text_on_screen(&h, "InviteWelcome::member-count-text").as_deref(),
        Some("4 members")
    );
    assert_eq!(
        ElementHandle::find_by_element_type_name(h.app(), "UserAvatar").count(),
        3,
        "the inviter and the two member previews are octagons"
    );
    assert_eq!(
        step_indicator(&h),
        None,
        "the welcome screen has no step indicator"
    );
    assert!(!h.controls_labelled("Join Night Stones").is_empty());
    assert!(!h
        .controls_labelled("Not now — show me other crews")
        .is_empty());
    assert!(!join_crew_modal_is_visible(&h));

    // A late crew list does not move the user off the welcome screen.
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });
    assert!(welcome_is_visible(&h));
}

/// ★ "Join" opens step 2, which counts two steps: the invite skipped step 1.
/// Step 3 is then "STEP 02 / 02".
#[test]
fn join_on_the_welcome_screen_opens_step_one_of_two() {
    let mut h = Harness::new();
    accept_invite_on_fresh_install(&mut h);

    assert!(!welcome_is_visible(&h));
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 01 / 02"));
    assert_eq!(
        text_on_screen(&h, "Onboarding::invite-crew-text").as_deref(),
        Some("Night Stones"),
        "step 2 keeps the JOINING CREW line"
    );

    // The indicator counts from step 2: its first mark is step 2 itself.
    h.click_label("Go to step 1");
    assert_eq!(h.app().get_onboarding_step(), 2);

    finalize(&h);
    h.emit(Event::OnboardingReady {
        user: sample_user(),
    });
    assert_eq!(h.app().get_onboarding_step(), 3);
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 02 / 02"));

    // A restart on step 3 keeps the count.
    let h2 = h.restart();
    assert_eq!(h2.app().get_onboarding_step(), 3);
    assert_eq!(step_indicator(&h2).as_deref(), Some("STEP 02 / 02"));
}

/// Without an invite the steps count to three, as before.
#[test]
fn without_an_invite_onboarding_counts_three_steps() {
    let mut h = Harness::new();
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 01 / 03"));
    h.app().invoke_onboarding_crew_selected("crew-0".into());
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 02 / 03"));
    finalize(&h);
    h.emit(Event::OnboardingReady {
        user: sample_user(),
    });
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 03 / 03"));
}

/// ★ "Not now" opens step 1 and forgets the invite. Finishing onboarding then
/// does not join the invited crew.
#[test]
fn not_now_on_the_welcome_screen_forgets_the_invite() {
    let mut h = Harness::new();
    welcome_on_fresh_install(&mut h, sample_invite());
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });

    h.click_label("Not now — show me other crews");
    assert_eq!(h.app().get_onboarding_step(), 1, "step 1: the other crews");
    assert!(!welcome_is_visible(&h));
    {
        let settings = h.settings();
        let s = settings.borrow();
        assert!(s.pending_invite_code.is_none(), "the invite is forgotten");
        assert!(s.pending_invite.is_none());
        assert!(!s.onboarding_via_invite);
    }
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 01 / 03"));

    h.app().invoke_onboarding_crew_selected("crew-0".into());
    assert_eq!(
        text_on_screen(&h, "Onboarding::invite-crew-text"),
        None,
        "step 2 does not show the invited crew"
    );
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 02 / 03"));
    h.commands();
    finalize(&h);
    assert_eq!(
        finalize_invite(&h.commands()),
        Some((None, Some("crew-0".into()), None)),
        "finalize sends no invite code: it joins the crew picked at step 1"
    );
    h.emit(Event::OnboardingReady {
        user: sample_user(),
    });
    let after = h.commands();
    assert!(
        !after
            .iter()
            .any(|c| matches!(c, Command::ResolveCrewInvite { .. })),
        "the declined invite must not come back as a join modal: {after:?}"
    );
}

/// With no inviter, the welcome screen still says the user is invited, with
/// no name and no pronoun.
#[test]
fn the_welcome_screen_without_an_inviter_says_you_are_invited() {
    let mut h = Harness::new();
    welcome_on_fresh_install(
        &mut h,
        mello_core::crew::ResolvedInvite {
            inviter: None,
            ..sample_invite()
        },
    );

    assert!(welcome_is_visible(&h));
    assert_eq!(
        text_on_screen(&h, "InviteWelcome::inviter-text").as_deref(),
        Some("You're invited to join")
    );
    assert_eq!(
        ElementHandle::find_by_element_type_name(h.app(), "UserAvatar").count(),
        2,
        "no inviter octagon, only the member previews"
    );
}

/// ★ A restart on the welcome screen shows it again, from Settings: no
/// network call, and no session restore.
///
/// The welcome screen is step 5, after "done" (4). Startup used to read any
/// step above 3 as a finished onboarding and try to restore a session that
/// does not exist.
#[test]
fn a_restart_on_the_welcome_screen_shows_it_again() {
    let mut h = Harness::new();
    welcome_on_fresh_install(&mut h, sample_invite());
    assert!(welcome_is_visible(&h));

    let mut h2 = h.restart();
    let cmds = h2.commands();
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::TryRestore)),
        "no account exists yet: nothing to restore, got {cmds:?}"
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::ResolveCrewInvite { .. })),
        "the stored invite shows without a network call: {cmds:?}"
    );
    assert_eq!(h2.app().get_onboarding_step(), 5);
    assert!(welcome_is_visible(&h2), "the welcome screen shows again");
    assert_eq!(
        text_on_screen(&h2, "InviteWelcome::inviter-text").as_deref(),
        Some("alice invited you to join")
    );
    assert_eq!(
        text_on_screen(&h2, "InviteWelcome::crew-name-text").as_deref(),
        Some("Night Stones")
    );

    // "Join" still works after the restart, and finalize joins by the code.
    h2.click_label("Join Night Stones");
    assert_eq!(step_indicator(&h2).as_deref(), Some("STEP 01 / 02"));
    h2.commands();
    finalize(&h2);
    assert_eq!(
        finalize_invite(&h2.commands()),
        Some((Some("NITE-0001".into()), None, None))
    );
}

/// A welcome screen persisted with no stored invite has nothing to show:
/// startup opens step 1.
#[test]
fn a_restart_on_the_welcome_screen_without_an_invite_opens_step_one() {
    let h = Harness::new();
    h.settings().borrow_mut().onboarding_step = 5;

    let h2 = h.restart();
    assert_eq!(h2.app().get_onboarding_step(), 1);
    assert!(!welcome_is_visible(&h2));
}

/// The retry behavior of finalize holds with an invite: one device id, the
/// same code on each attempt.
#[test]
fn retrying_finalize_with_an_invite_keeps_the_device_id_and_the_code() {
    let mut h = Harness::new();
    accept_invite_on_fresh_install(&mut h);

    finalize(&h);
    let first = h.commands();
    h.emit(Event::OnboardingFailed {
        reason: "Connection failed: timed out".into(),
        join_error: None,
    });
    finalize(&h);
    let second = h.commands();

    assert_eq!(finalize_device_ids(&first), finalize_device_ids(&second));
    assert_eq!(
        finalize_invite(&second),
        Some((Some("NITE-0001".into()), None, None))
    );
}

// ---------------------------------------------------------------------------
// The lounge invite on the clipboard, and the lounge's "Open in m3llo" button
// ---------------------------------------------------------------------------

/// A clipboard for startup that holds `text` and records whether startup read
/// it. The lounge host is the release one.
fn clipboard_with(h: &Harness, text: &'static str) -> std::rc::Rc<std::cell::Cell<bool>> {
    let read = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = read.clone();
    *h.ctx().startup_clipboard.borrow_mut() = Some(crate::onboarding_invite::StartupClipboard {
        lounge_host: "m3llo.app".into(),
        read: Box::new(move || {
            flag.set(true);
            Some(text.to_string())
        }),
    });
    read
}

fn resolved_codes(cmds: &[Command]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::ResolveCrewInvite { code } => Some(code.clone()),
            _ => None,
        })
        .collect()
}

/// ★ The lounge copies the invite link when the user downloads the app. The
/// first launch of the fresh install finds it on the clipboard and opens the
/// welcome screen for that crew, with no deep link.
#[test]
fn a_lounge_link_on_the_clipboard_opens_the_welcome_screen_on_first_launch() {
    let mut h = Harness::new();
    let read = clipboard_with(&h, "https://m3llo.app/join/NITE-0001");

    crate::onboarding::start(h.ctx());
    let cmds = h.commands();
    assert!(read.get(), "a fresh install reads the clipboard");
    let resolve = cmds
        .iter()
        .position(|c| matches!(c, Command::ResolveCrewInvite { code } if code == "NITE-0001"));
    let discover = cmds
        .iter()
        .position(|c| matches!(c, Command::DiscoverCrews { .. }));
    assert!(
        resolve.is_some() && resolve < discover,
        "the invite resolves before crew discovery, as a deep link does: {cmds:?}"
    );

    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });
    assert!(welcome_is_visible(&h), "the welcome screen for the crew");
    assert_eq!(h.app().get_onboarding_step(), 5);
}

/// Anything on the clipboard that is not a join link on the lounge host is
/// ignored. Startup opens step 1 as usual.
#[test]
fn other_clipboard_text_is_ignored() {
    for text in [
        "hunter2",
        "NITE-0001",
        "mello://join/NITE-0001",
        "https://example.com/join/NITE-0001",
    ] {
        let mut h = Harness::new();
        let read = clipboard_with(&h, text);
        crate::onboarding::start(h.ctx());
        assert!(read.get());
        let cmds = h.commands();
        assert!(resolved_codes(&cmds).is_empty(), "{text:?}: {cmds:?}");

        h.emit(Event::DiscoverCrewsLoaded {
            crews: sample_crews(3),
            cursor: None,
        });
        assert_eq!(h.app().get_onboarding_step(), 1, "{text:?}");
    }
}

/// Startup reads the clipboard only on the first launch of a fresh install,
/// and only with no deep link.
#[test]
fn startup_reads_the_clipboard_only_on_a_first_launch_with_no_deep_link() {
    // A deep link arrived: it wins, and the clipboard is not read.
    let mut h = Harness::new();
    let read = clipboard_with(&h, "https://m3llo.app/join/CLIP-0001");
    *h.ctx().pending_deep_link.borrow_mut() = Some(crate::deep_link::DeepLink::Join {
        code: "NITE-0001".into(),
    });
    crate::onboarding::start(h.ctx());
    assert!(!read.get(), "a deep link arrived: no clipboard read");
    assert_eq!(resolved_codes(&h.commands()), vec!["NITE-0001".to_string()]);

    // A device account: not a fresh install.
    let mut h = Harness::new();
    h.settings().borrow_mut().device_id = Some("dev-abc".into());
    h.settings().borrow_mut().onboarding_step = 2;
    let read = clipboard_with(&h, "https://m3llo.app/join/CLIP-0001");
    crate::onboarding::start(h.ctx());
    assert!(!read.get(), "a device account: no clipboard read");
    assert!(resolved_codes(&h.commands()).is_empty());

    // A later launch on step 1, for example after "Not now": the declined
    // invite must not open again.
    let mut h = Harness::new();
    h.settings().borrow_mut().onboarding_step = 1;
    let read = clipboard_with(&h, "https://m3llo.app/join/CLIP-0001");
    crate::onboarding::start(h.ctx());
    assert!(!read.get(), "not the first launch: no clipboard read");
    assert!(resolved_codes(&h.commands()).is_empty());

    // A returning user.
    let h = Harness::new();
    h.settings().borrow_mut().onboarding_step = 4;
    let read = clipboard_with(&h, "https://m3llo.app/join/CLIP-0001");
    crate::onboarding::start(h.ctx());
    assert!(!read.get(), "a returning user: no clipboard read");
}

/// ★ CREW-INVITES §7: the installer starts the app, which opens step 1 with no
/// invite (the user copied something else). The user then presses "Open in
/// m3llo" in the lounge. The OS starts a second instance, which relays the
/// link to the running app over the real IPC endpoint. The running app must
/// open the welcome screen, not the join modal.
#[test]
fn a_relayed_join_link_on_step_one_opens_the_welcome_screen() {
    let mut h = Harness::new();
    let endpoint =
        crate::ipc::endpoint_name(&format!("mello-relay-step-one.{}", std::process::id()));
    let listener = crate::ipc::IpcListener::bind(&endpoint).expect("bind the IPC endpoint");
    *h.ctx().ipc_listener.borrow_mut() = Some(listener);
    step_one_fresh_install(&mut h);

    // What the second instance does in `lib.rs`.
    assert!(crate::ipc::send_to_running(
        &endpoint,
        "mello://join/NITE-0001"
    ));

    // The listener hands the line over on its own thread (Windows). Wait for
    // the command that the test asserts on, with a limit.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut codes = Vec::new();
    while codes.is_empty() && std::time::Instant::now() < deadline {
        h.pump();
        codes = resolved_codes(&h.commands());
        if codes.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    assert_eq!(
        codes,
        vec!["NITE-0001".to_string()],
        "the running app resolves the link"
    );

    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });
    assert!(welcome_is_visible(&h), "the welcome screen for the crew");
    assert_eq!(h.app().get_onboarding_step(), 5);
    assert!(!join_crew_modal_is_visible(&h), "no join modal on step 1");
    assert_eq!(
        h.settings().borrow().pending_invite_code.as_deref(),
        Some("NITE-0001"),
        "finalize joins this crew"
    );
}

/// INV-05: an invalid invite code on a fresh install shows step 1 with a
/// message. Step 1 offers a way forward. No join modal opens.
#[test]
fn an_invalid_invite_on_a_fresh_install_shows_step_one_with_a_message() {
    let mut h = Harness::new();
    start_fresh_install_from_invite(&mut h);

    h.emit(Event::CrewInviteResolveFailed {
        reason: "invalid invite code".into(),
        error: mello_core::crew::InviteError::InvalidCode,
    });
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });

    assert_eq!(h.app().get_onboarding_step(), 1);
    assert!(!join_crew_modal_is_visible(&h));
    assert_eq!(
        text_on_screen(&h, "Onboarding::invite-error-text").as_deref(),
        Some("This invite link is no longer valid.")
    );
    assert_eq!(
        ElementHandle::find_by_element_type_name(h.app(), "CreateCrewCard").count(),
        1,
        "step 1 still offers a way forward"
    );

    // Picking a crew clears the message and moves on.
    h.app().invoke_onboarding_crew_selected("crew-0".into());
    assert_eq!(h.app().get_onboarding_step(), 2);
    assert_eq!(h.app().get_onboarding_invite_error().as_str(), "");
}

/// A crew picked at step 1 replaces the invite.
#[test]
fn a_crew_picked_at_step_one_replaces_the_invite() {
    let mut h = Harness::new();
    accept_invite_on_fresh_install(&mut h);

    h.app().invoke_onboarding_continue(1);
    assert_eq!(
        h.app().get_onboarding_step(),
        1,
        "the step indicator goes back"
    );
    h.app().invoke_onboarding_crew_selected("crew-0".into());
    assert_eq!(h.app().get_onboarding_step(), 2);
    assert!(
        text_on_screen(&h, "Onboarding::invite-crew-text").is_none(),
        "step 2 no longer shows the invited crew"
    );

    h.commands();
    finalize(&h);
    assert_eq!(
        finalize_invite(&h.commands()),
        Some((None, Some("crew-0".into()), None))
    );
}

/// Finalize could not join the invited crew. A transient failure stays on
/// step 2 with a retry. A crew that cannot take the user goes back to step 1.
#[test]
fn a_failed_invite_join_at_finalize_is_never_a_dead_end() {
    let mut h = Harness::new();
    accept_invite_on_fresh_install(&mut h);

    finalize(&h);
    h.emit(Event::OnboardingInviteFailed {
        error: mello_core::crew::InviteError::Failed,
    });
    assert_eq!(h.app().get_onboarding_step(), 2);
    assert!(!h.app().get_onboarding_busy(), "Continue accepts a retry");
    assert_eq!(
        text_on_screen(&h, "Onboarding::step2-error-text").as_deref(),
        Some("Could not join the crew. Try again.")
    );

    finalize(&h);
    assert_eq!(
        text_on_screen(&h, "Onboarding::step2-error-text"),
        None,
        "a retry clears the previous error"
    );
    h.emit(Event::OnboardingInviteFailed {
        error: mello_core::crew::InviteError::CrewFull,
    });
    assert_eq!(
        h.app().get_onboarding_step(),
        1,
        "the user picks another crew"
    );
    assert!(!h.app().get_onboarding_busy());
    assert_eq!(
        text_on_screen(&h, "Onboarding::invite-error-text").as_deref(),
        Some("This crew is full.")
    );
    assert!(h.settings().borrow().pending_invite_code.is_none());
}

/// A machine with a device account keeps today's path: the link waits for
/// sign-in and then opens the join modal.
#[test]
fn an_invite_on_a_machine_with_a_device_account_waits_for_sign_in() {
    let mut h = Harness::new();
    h.settings().borrow_mut().device_id = Some("dev-abc".into());
    *h.ctx().pending_deep_link.borrow_mut() = Some(crate::deep_link::DeepLink::Join {
        code: "NITE-0001".into(),
    });

    crate::onboarding_invite::dispatch_at_startup(
        h.ctx(),
        crate::onboarding::OnboardingState::PickCrew,
    );
    let cmds = h.commands();
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::ResolveCrewInvite { .. })),
        "{cmds:?}"
    );
    assert!(h.ctx().pending_deep_link.borrow().is_some());
}

/// A restart on step 2 still shows the invited crew and still joins it.
#[test]
fn a_restart_on_step_two_keeps_the_invite() {
    let mut h = Harness::new();
    {
        let settings = h.settings();
        let mut s = settings.borrow_mut();
        s.pending_invite_code = Some("NITE-0001".into());
        s.pending_invite_crew_name = Some("Night Stones".into());
    }

    crate::onboarding_invite::dispatch_at_startup(
        h.ctx(),
        crate::onboarding::OnboardingState::PickAvatar,
    );
    crate::onboarding::resume(h.ctx(), crate::onboarding::OnboardingState::PickAvatar);
    h.pump();

    assert_eq!(
        text_on_screen(&h, "Onboarding::invite-crew-text").as_deref(),
        Some("Night Stones")
    );
    h.commands();
    finalize(&h);
    assert_eq!(
        finalize_invite(&h.commands()),
        Some((Some("NITE-0001".into()), None, None))
    );
}

/// core → UI: joining a crew must make the app screen usable rather than
/// leaving the user in a half-populated state.
#[test]
fn crews_loaded_populates_the_sidebar() {
    let mut h = Harness::new();
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });

    h.emit(Event::CrewsLoaded {
        crews: sample_crews(2),
    });

    assert_eq!(visible_screens(&h), vec![Screen::App]);
    h.assert_not_blank();
}

/// UI → core: the mute path, end to end through the real wiring.
#[test]
fn mute_toggle_emits_set_mute_and_broadcast() {
    let mut h = Harness::new();

    h.app().invoke_mic_toggle();

    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::SetMute { muted: true })),
        "expected SetMute, got {cmds:?}"
    );
}

// ---------------------------------------------------------------------------
// The invite-code card on step 1
// ---------------------------------------------------------------------------

/// The card's field: its label is the caption the user reads.
const CARD_FIELD: &str = "GOT AN INVITE?";
const OPEN_INVITE: &str = "Open invite";
const CHECKING: &str = "Checking…";
const CODE_NOT_VALID: &str = "This invite code is not valid.";
const COULD_NOT_LOAD: &str = "Could not load this invite. Try again.";

/// Step 1 of a fresh install with a list of crews and no deep link.
fn step_one_fresh_install(h: &mut Harness) {
    crate::onboarding::resume(h.ctx(), crate::onboarding::OnboardingState::Loading);
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });
    assert_eq!(h.app().get_onboarding_step(), 1);
    h.commands();
}

/// Focus the card's field with a click, then type as the user does.
fn type_in_card(h: &mut Harness, text: &str) {
    let field = h.controls_labelled(CARD_FIELD);
    assert_eq!(field.len(), 1, "the card has one field on step 1");
    field[0].mock_single_click(slint::platform::PointerEventButton::Left);
    h.type_text(text);
}

fn resolve_commands(cmds: &[Command]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::ResolveCrewInvite { code } => Some(code.clone()),
            _ => None,
        })
        .collect()
}

fn card_error(h: &Harness) -> String {
    h.app().get_onboarding_invite_code_error().to_string()
}

/// A crew avatar as core delivers it: base64 of an image file.
fn avatar_data() -> String {
    let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 60, 60, 255]));
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png)
        .expect("a PNG encodes in memory");
    base64::engine::general_purpose::STANDARD.encode(png.into_inner())
}

/// ★ A user whose link the web lounge could not hand to the app pastes it
/// into the card on step 1. The resolve takes the path of a deep link: the
/// welcome screen opens, and finalize joins the crew by its invite code.
///
/// Before the card, this user had no way to reach the invited crew.
#[test]
fn a_link_typed_in_the_card_opens_the_welcome_screen() {
    let mut h = Harness::new();
    step_one_fresh_install(&mut h);
    assert!(!h.controls_labelled(OPEN_INVITE).is_empty());

    type_in_card(&mut h, "https://m3llo.app/join/nite-0001");
    h.click_label(OPEN_INVITE);

    let cmds = h.commands();
    assert_eq!(
        resolve_commands(&cmds),
        vec!["NITE-0001".to_string()],
        "the card sends the code, normalised: {cmds:?}"
    );
    assert!(
        h.controls_labelled(OPEN_INVITE).is_empty() && !h.controls_labelled(CHECKING).is_empty(),
        "the button reads \"Checking…\" while the resolve runs"
    );

    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });
    assert!(
        welcome_is_visible(&h),
        "the welcome screen opens, as for a deep link"
    );
    assert!(!join_crew_modal_is_visible(&h));
    assert_eq!(
        text_on_screen(&h, "InviteWelcome::inviter-text").as_deref(),
        Some("alice invited you to join")
    );
    assert!(h.settings().borrow().onboarding_via_invite);
    assert_eq!(
        h.settings().borrow().pending_invite_code.as_deref(),
        Some("NITE-0001")
    );

    h.click_label("Join Night Stones");
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 01 / 02"));
    h.commands();
    finalize(&h);
    assert_eq!(
        finalize_invite(&h.commands()),
        Some((Some("NITE-0001".into()), None, None)),
        "finalize joins the invited crew by its code"
    );
}

/// The card takes a bare code, and Enter in the field submits it.
#[test]
fn enter_in_the_card_field_sends_the_code() {
    let mut h = Harness::new();
    step_one_fresh_install(&mut h);

    type_in_card(&mut h, "  nite0001 \n");

    assert_eq!(resolve_commands(&h.commands()), vec!["NITE-0001"]);
    assert!(h.app().get_onboarding_invite_code_checking());
}

/// ★ While the resolve runs, the button ignores clicks and Enter: one
/// command, not one for each press.
#[test]
fn the_card_sends_one_resolve_while_it_waits() {
    let mut h = Harness::new();
    step_one_fresh_install(&mut h);
    type_in_card(&mut h, "NITE-0001");
    h.click_label(OPEN_INVITE);
    assert_eq!(resolve_commands(&h.commands()).len(), 1);

    h.click_label(CHECKING);
    type_in_card(&mut h, "\n");
    // The window also guards the callback itself.
    h.app().invoke_onboarding_open_invite("NITE-0001".into());

    assert_eq!(
        resolve_commands(&h.commands()),
        Vec::<String>::new(),
        "no second resolve while the first runs"
    );
}

/// ★ Text that is no invite gets the message in the card, and no network
/// call.
#[test]
fn an_invalid_input_in_the_card_shows_the_error_and_sends_nothing() {
    let mut h = Harness::new();
    step_one_fresh_install(&mut h);

    for input in ["hello world", "ABCD", "https://example.com/join/NITE-0001"] {
        type_in_card(&mut h, input);
        h.click_label(OPEN_INVITE);
        assert_eq!(card_error(&h), CODE_NOT_VALID, "{input:?}");
        assert_eq!(
            text_on_screen(&h, "InviteCodeCard::error-text").as_deref(),
            Some(CODE_NOT_VALID),
            "{input:?}: the message is under the field"
        );
        assert!(!h.app().get_onboarding_invite_code_checking());
        let cmds = h.commands();
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Command::ResolveCrewInvite { .. })),
            "{input:?}: no command for an input that is no invite: {cmds:?}"
        );
        // Clear the field for the next input.
        h.app().set_onboarding_invite_code_text("".into());
    }
}

/// ★ A resolve failure shows its own text in the card, not above the crews,
/// and the button accepts a new try.
#[test]
fn a_resolve_failure_shows_in_the_card() {
    for (error, text) in [
        (mello_core::crew::InviteError::InvalidCode, CODE_NOT_VALID),
        (mello_core::crew::InviteError::Failed, COULD_NOT_LOAD),
    ] {
        let mut h = Harness::new();
        step_one_fresh_install(&mut h);
        type_in_card(&mut h, "NITE-0001");
        h.click_label(OPEN_INVITE);
        h.commands();

        h.emit(Event::CrewInviteResolveFailed {
            reason: "server said no".into(),
            error,
        });

        assert_eq!(card_error(&h), text, "{error:?}");
        assert_eq!(
            text_on_screen(&h, "InviteCodeCard::error-text").as_deref(),
            Some(text)
        );
        assert_eq!(
            h.app().get_onboarding_invite_error().as_str(),
            "",
            "the message stays in the card: nothing above the crews"
        );
        assert!(!join_crew_modal_is_visible(&h));
        assert_eq!(h.app().get_onboarding_step(), 1);
        assert!(
            !h.controls_labelled(OPEN_INVITE).is_empty(),
            "the button reads \"Open invite\" again"
        );
        h.click_label(OPEN_INVITE);
        assert_eq!(
            resolve_commands(&h.commands()),
            vec!["NITE-0001"],
            "the user can try again"
        );
    }
}

/// ★ Editing the field clears the error.
#[test]
fn editing_the_card_field_clears_the_error() {
    let mut h = Harness::new();
    step_one_fresh_install(&mut h);
    type_in_card(&mut h, "WXYZ-0000");
    h.click_label(OPEN_INVITE);
    assert_eq!(card_error(&h), "", "a well-formed code goes to the network");
    h.emit(Event::CrewInviteResolveFailed {
        reason: "not found".into(),
        error: mello_core::crew::InviteError::InvalidCode,
    });
    assert_eq!(card_error(&h), CODE_NOT_VALID);

    h.type_text("1");

    assert_eq!(card_error(&h), "", "the user edited the field");
}

/// A link that opened the app still shows its message above the crews. The
/// card does not take it.
#[test]
fn a_deep_link_failure_still_shows_above_the_crews() {
    let mut h = Harness::new();
    start_fresh_install_from_invite(&mut h);

    h.emit(Event::CrewInviteResolveFailed {
        reason: "not found".into(),
        error: mello_core::crew::InviteError::InvalidCode,
    });

    assert_eq!(
        h.app().get_onboarding_invite_error().as_str(),
        "This invite link is no longer valid."
    );
    assert_eq!(card_error(&h), "");
}

/// ★ "Not now" returns to step 1 with the field cleared.
#[test]
fn not_now_after_the_card_returns_to_step_one_with_the_field_cleared() {
    let mut h = Harness::new();
    step_one_fresh_install(&mut h);
    type_in_card(&mut h, "https://m3llo.app/join/NITE-0001");
    h.click_label(OPEN_INVITE);
    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });
    assert!(welcome_is_visible(&h));
    assert_eq!(
        h.app().get_onboarding_invite_code_text().as_str(),
        "https://m3llo.app/join/NITE-0001",
        "the text stays while the welcome screen shows"
    );

    h.click_label("Not now — show me other crews");

    assert_eq!(h.app().get_onboarding_step(), 1);
    assert_eq!(h.app().get_onboarding_invite_code_text().as_str(), "");
    assert_eq!(card_error(&h), "");
    assert!(!h.app().get_onboarding_invite_code_checking());
    let fields = h.controls_labelled(CARD_FIELD);
    assert_eq!(fields.len(), 1, "the card is on step 1");
    assert_eq!(
        fields[0].accessible_value().unwrap_or_default().as_str(),
        "",
        "the field on screen is empty"
    );
}

/// A user with no discoverable crews still has the card, next to "Create
/// your own crew".
#[test]
fn the_card_is_on_step_one_with_no_discoverable_crews() {
    let mut h = Harness::new();
    crate::onboarding::resume(h.ctx(), crate::onboarding::OnboardingState::Loading);
    h.emit(Event::DiscoverCrewsLoaded {
        crews: vec![],
        cursor: None,
    });

    assert_eq!(h.app().get_onboarding_step(), 1);
    assert_eq!(
        ElementHandle::find_by_element_type_name(h.app(), "InviteCodeCard").count(),
        1
    );
    assert_eq!(
        ElementHandle::find_by_element_type_name(h.app(), "CreateCrewCard").count(),
        1
    );
    assert!(!h.controls_labelled(CARD_FIELD).is_empty());
}

/// The card is on step 1 once, whatever the number of crews.
#[test]
fn the_card_is_on_step_one_once_with_crews() {
    let mut h = Harness::new();
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(8),
        cursor: None,
    });

    assert_eq!(
        ElementHandle::find_by_element_type_name(h.app(), "InviteCodeCard").count(),
        1
    );
}

/// ★ The card takes the place of a crew card in row 1. No crew that step 1
/// shows loses its card because of it: four crews, four cards, and the
/// "Create your own crew" card.
#[test]
fn the_card_leaves_a_card_for_each_crew_that_step_one_shows() {
    let mut h = Harness::new();
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(6),
        cursor: None,
    });

    let names: Vec<_> = ElementHandle::find_by_element_type_name(h.app(), "CrewCard")
        .filter_map(|e| e.accessible_label())
        .collect();
    assert_eq!(
        ElementHandle::find_by_element_type_name(h.app(), "CrewCard").count(),
        4,
        "four crews, one card each: {names:?}"
    );
    assert_eq!(
        ElementHandle::find_by_element_type_name(h.app(), "CreateCrewCard").count(),
        1
    );
    let model = h.app().get_discover_crews();
    assert_eq!(model.row_count(), 4, "step 1 keeps four crews");
}

/// ★ After a logout the machine has a device account, and the user is on
/// step 1 with no session. The card takes the same path: the welcome screen
/// opens (no join modal, which needs a session), and finalize joins the crew
/// by its code, through device auth into the existing account.
#[test]
fn after_a_logout_the_card_joins_the_crew_into_the_device_account() {
    let mut h = Harness::new();
    with_device_account(&h);
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.app().invoke_logout();
    assert!(!h.app().get_logged_in());
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });
    assert_eq!(h.app().get_onboarding_step(), 1);
    assert!(h.settings().borrow().has_device_account());
    h.commands();

    type_in_card(&mut h, "nite-0001");
    h.click_label(OPEN_INVITE);
    assert_eq!(resolve_commands(&h.commands()), vec!["NITE-0001"]);
    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });

    assert!(welcome_is_visible(&h), "the welcome screen, not the modal");
    assert!(!join_crew_modal_is_visible(&h));
    h.click_label("Join Night Stones");
    assert_eq!(h.app().get_onboarding_step(), 2);
    h.commands();
    finalize(&h);
    let cmds = h.commands();
    let finalize_cmd = cmds.iter().find_map(|c| match c {
        Command::FinalizeOnboarding {
            device_id,
            invite_code,
            crew_id,
            crew_name,
            ..
        } => Some((
            device_id.clone(),
            invite_code.clone(),
            crew_id.clone(),
            crew_name.clone(),
        )),
        _ => None,
    });
    assert_eq!(
        finalize_cmd,
        Some(("dev-abc".into(), Some("NITE-0001".into()), None, None)),
        "the existing device id, and the invite code: {cmds:?}"
    );

    h.emit(Event::OnboardingReady {
        user: sample_user(),
    });
    assert_eq!(h.app().get_onboarding_step(), 3);
    let after = h.commands();
    assert!(
        resolve_commands(&after).is_empty(),
        "the invite is used: it does not come back as a join modal: {after:?}"
    );
}

/// The card only changes where an invite that the user typed goes. A link
/// that resolves for a device account with no card in use keeps the join
/// modal path (CREW-INVITES §7).
#[test]
fn a_resolve_the_card_did_not_send_keeps_the_device_account_path() {
    let mut h = Harness::new();
    with_device_account(&h);
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.app().invoke_logout();
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });

    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });

    assert!(!welcome_is_visible(&h));
    assert!(join_crew_modal_is_visible(&h));
}

/// ★ "Back" on step 2 of the invite path returns to the welcome screen.
/// The invite is kept: "Join" opens step 2 again and finalize joins by code.
#[test]
fn back_on_step_two_of_the_invite_path_returns_to_the_welcome_screen() {
    let mut h = Harness::new();
    accept_invite_on_fresh_install(&mut h);
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 01 / 02"));

    h.click_label("Back");

    assert_eq!(h.app().get_onboarding_step(), 5, "the welcome screen");
    assert!(welcome_is_visible(&h));
    assert_eq!(
        text_on_screen(&h, "InviteWelcome::crew-name-text").as_deref(),
        Some("Night Stones")
    );
    assert_eq!(
        text_on_screen(&h, "InviteWelcome::inviter-text").as_deref(),
        Some("alice invited you to join")
    );
    {
        let settings = h.settings();
        let s = settings.borrow();
        assert_eq!(s.pending_invite_code.as_deref(), Some("NITE-0001"));
        assert!(s.pending_invite.is_some());
        assert!(s.onboarding_via_invite, "still the invite path");
        assert_eq!(s.onboarding_step, 5, "persisted, so a restart shows it");
    }

    h.click_label("Join Night Stones");
    assert_eq!(step_indicator(&h).as_deref(), Some("STEP 01 / 02"));
    h.commands();
    finalize(&h);
    assert_eq!(
        finalize_invite(&h.commands()),
        Some((Some("NITE-0001".into()), None, None))
    );
}

/// "Back" belongs to the invite path. Step 2 with a crew picked at step 1
/// has the step indicator to go back, and no "Back".
#[test]
fn step_two_has_no_back_control_off_the_invite_path() {
    let mut h = Harness::new();
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });
    h.app().invoke_onboarding_crew_selected("crew-0".into());
    assert_eq!(h.app().get_onboarding_step(), 2);

    assert!(h.controls_labelled("Back").is_empty());
}

/// A finalize in flight cannot be left: "Back" does nothing while busy.
#[test]
fn back_does_nothing_while_the_account_is_being_created() {
    let mut h = Harness::new();
    accept_invite_on_fresh_install(&mut h);
    h.app().set_onboarding_busy(true);

    h.click_label("Back");

    assert_eq!(h.app().get_onboarding_step(), 2);
}

/// ★ The Discover field takes a link, as the card does. Before, only a bare
/// code in the exact stored form worked.
#[test]
fn the_discover_code_field_accepts_a_link() {
    let mut h = Harness::new();
    for input in [
        "https://m3llo.app/join/nite-0001",
        "m3llo.app/join/NITE-0001/",
        "mello://join/NITE-0001",
        "  nite0001  ",
    ] {
        h.app().invoke_discover_join_invite(input.into());
        let cmds = h.commands();
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::JoinByInviteCode { code } if code == "NITE-0001")),
            "{input:?} must join by NITE-0001, got {cmds:?}"
        );
        assert_eq!(h.app().get_discover_invite_error().as_str(), "");
    }
}

/// An input that is no invite gets the message under the Discover field, and
/// no command.
#[test]
fn the_discover_code_field_refuses_what_is_no_invite() {
    let mut h = Harness::new();

    h.app()
        .invoke_discover_join_invite("https://example.com/join/NITE-0001".into());

    assert_eq!(h.app().get_discover_invite_error().as_str(), CODE_NOT_VALID);
    let cmds = h.commands();
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::JoinByInviteCode { .. })),
        "{cmds:?}"
    );
}

/// ★ The crew avatar loads by crew ID, without a session, and shows on the
/// welcome screen and on step 2. Initials show until it arrives.
#[test]
fn the_invited_crew_avatar_loads_and_shows_on_the_welcome_screen_and_step_two() {
    let mut h = Harness::new();
    start_fresh_install_from_invite(&mut h);
    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });

    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::FetchCrewAvatars { crew_ids } if crew_ids == &["crew-inv".to_string()]
        )),
        "the avatar is fetched by crew ID: {cmds:?}"
    );
    assert!(
        !h.app().get_onboarding_invite_crew_has_avatar(),
        "initials show until the avatar arrives"
    );
    assert_eq!(h.app().get_onboarding_invite_crew_initials().as_str(), "NS");

    // The avatar of another crew is not the invited crew's.
    h.emit(Event::CrewAvatarLoaded {
        crew_id: "crew-other".into(),
        data: avatar_data(),
    });
    assert!(!h.app().get_onboarding_invite_crew_has_avatar());

    h.emit(Event::CrewAvatarLoaded {
        crew_id: "crew-inv".into(),
        data: avatar_data(),
    });
    assert!(h.app().get_onboarding_invite_crew_has_avatar());
    assert!(welcome_is_visible(&h));

    // Step 2 shows the same avatar, and "Back" keeps it.
    h.click_label("Join Night Stones");
    assert!(h.app().get_onboarding_invite_crew_has_avatar());
    h.click_label("Back");
    assert!(h.app().get_onboarding_invite_crew_has_avatar());
}

/// "Not now" forgets the invite, and its avatar with it. A second invite
/// starts with initials again.
#[test]
fn the_invited_crew_avatar_does_not_outlive_its_invite() {
    let mut h = Harness::new();
    welcome_on_fresh_install(&mut h, sample_invite());
    h.emit(Event::CrewAvatarLoaded {
        crew_id: "crew-inv".into(),
        data: avatar_data(),
    });
    assert!(h.app().get_onboarding_invite_crew_has_avatar());

    // A second link replaces the first.
    h.emit(Event::CrewInviteResolved {
        code: "OTHR-0002".into(),
        invite: mello_core::crew::ResolvedInvite {
            crew_id: "crew-2".into(),
            crew_name: "Other Crew".into(),
            ..sample_invite()
        },
    });
    assert!(!h.app().get_onboarding_invite_crew_has_avatar());

    h.emit(Event::CrewAvatarLoaded {
        crew_id: "crew-2".into(),
        data: avatar_data(),
    });
    assert!(h.app().get_onboarding_invite_crew_has_avatar());
    h.click_label("Not now — show me other crews");
    assert!(!h.app().get_onboarding_invite_crew_has_avatar());
}

/// A restart on the welcome screen loads the avatar again: only the invite
/// is stored, not the image.
#[test]
fn a_restart_on_the_welcome_screen_loads_the_avatar_again() {
    let mut h = Harness::new();
    welcome_on_fresh_install(&mut h, sample_invite());

    let mut h2 = h.restart();
    let cmds = h2.commands();

    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::FetchCrewAvatars { crew_ids } if crew_ids == &["crew-inv".to_string()]
        )),
        "{cmds:?}"
    );
}

/// The join modal shows the crew avatar too. The modal opens for a user with
/// an account, and the avatar loads by crew ID.
#[test]
fn the_join_modal_loads_and_shows_the_crew_avatar() {
    let mut h = Harness::new();
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.commands();

    h.emit(Event::CrewInviteResolved {
        code: "NITE-0001".into(),
        invite: sample_invite(),
    });
    assert!(join_crew_modal_is_visible(&h));
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::FetchCrewAvatars { crew_ids } if crew_ids == &["crew-inv".to_string()]
        )),
        "{cmds:?}"
    );
    assert!(
        !h.app().get_join_crew_has_avatar(),
        "initials show until it arrives"
    );

    h.emit(Event::CrewAvatarLoaded {
        crew_id: "crew-inv".into(),
        data: avatar_data(),
    });

    assert!(h.app().get_join_crew_has_avatar());
    assert!(
        !h.app().get_onboarding_invite_crew_has_avatar(),
        "the onboarding invite is not the modal's crew"
    );
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// An element can be present in the tree and still be invisible to the user if
/// it collapses to zero size. Structural queries cannot tell the difference, so
/// check the geometry of the controls a user must be able to hit.
///
/// Geometry is real under the headless backend: layout is computed even though
/// nothing is rasterised.
#[test]
fn primary_onboarding_controls_have_a_visible_size() {
    let mut h = Harness::new();
    h.emit(Event::DiscoverCrewsLoaded {
        crews: sample_crews(3),
        cursor: None,
    });

    for type_name in ["CrewCard", "CreateCrewCard"] {
        let elements: Vec<_> =
            ElementHandle::find_by_element_type_name(h.app(), type_name).collect();
        assert!(!elements.is_empty(), "expected at least one {type_name}");

        for (i, element) in elements.iter().enumerate() {
            let size = element.size();
            assert!(
                size.width > 1.0 && size.height > 1.0,
                "{type_name} {i} is {}x{} — present in the tree but too small to \
                 click, so the user cannot use it",
                size.width,
                size.height
            );
        }
    }
}

/// The same check for the discovery-failure screen, where the Retry button is
/// the user's only route back.
#[test]
fn discover_error_retry_control_has_a_visible_size() {
    let mut h = Harness::new();
    h.emit(Event::DiscoverCrewsFailed {
        reason: "connection refused".into(),
    });

    let create: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "CreateCrewCard").collect();
    assert!(!create.is_empty(), "the way forward must still be present");
    for element in &create {
        let size = element.size();
        assert!(
            size.width > 1.0 && size.height > 1.0,
            "CreateCrewCard collapsed to {}x{} on the error screen",
            size.width,
            size.height
        );
    }
}

// ---------------------------------------------------------------------------
// Auth entry points
// ---------------------------------------------------------------------------

/// Email sign-in must carry both fields, clear any previous error, and show
/// the spinner. A dropped password reaches the server as an empty one.
#[test]
fn email_login_carries_credentials_and_sets_loading() {
    let mut h = Harness::new();
    h.app().set_login_error("previous failure".into());

    h.app()
        .invoke_login("user@example.com".into(), "hunter2".into());

    assert!(h.app().get_login_loading(), "the spinner must appear");
    assert_eq!(
        h.app().get_login_error().as_str(),
        "",
        "a previous error must clear when retrying"
    );

    let cmds = h.commands();
    let sent = cmds.iter().find_map(|c| match c {
        Command::Login { email, password } => Some((email.clone(), password.clone())),
        _ => None,
    });
    assert_eq!(
        sent,
        Some(("user@example.com".to_string(), "hunter2".to_string())),
        "Login must carry both credentials, got {cmds:?}"
    );
}

/// Each social button must emit its own provider's command. Wiring two buttons
/// to the same command is an easy copy-paste slip and would send users to the
/// wrong identity provider.
#[test]
fn each_social_button_emits_its_own_provider() {
    /// (label, button to press, predicate matching that provider's command)
    type SocialCase = (&'static str, fn(&MainWindow), fn(&Command) -> bool);

    let cases: [SocialCase; 5] = [
        (
            "steam",
            |a| a.invoke_signin_steam(),
            |c| matches!(c, Command::AuthSteam),
        ),
        (
            "google",
            |a| a.invoke_signin_google(),
            |c| matches!(c, Command::AuthGoogle),
        ),
        (
            "twitch",
            |a| a.invoke_signin_twitch(),
            |c| matches!(c, Command::AuthTwitch),
        ),
        (
            "discord",
            |a| a.invoke_signin_discord(),
            |c| matches!(c, Command::AuthDiscord),
        ),
        (
            "apple",
            |a| a.invoke_signin_apple(),
            |c| matches!(c, Command::AuthApple { .. }),
        ),
    ];

    for (name, invoke, expected) in cases {
        let mut h = Harness::new();
        invoke(h.app());
        let cmds = h.commands();
        assert!(
            cmds.iter().any(expected),
            "the {name} button did not emit its own provider command, got {cmds:?}"
        );
        // Exactly one auth command, so a button cannot fire two providers.
        let auth_count = cmds
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    Command::AuthSteam
                        | Command::AuthGoogle
                        | Command::AuthTwitch
                        | Command::AuthDiscord
                        | Command::AuthApple { .. }
                )
            })
            .count();
        assert_eq!(auth_count, 1, "{name} emitted {auth_count} auth commands");
    }
}

/// ★ Regression (#67): social sign-in keeps the panel open while the provider
/// flow runs, so a failure shows on it with a way forward. The panel used to
/// close at once, and a failed sign-in landed on step 1 with no message.
#[test]
fn social_signin_keeps_the_panel_open_until_it_ends() {
    let mut h = Harness::new();
    h.app().set_show_sign_in(true);
    h.app().set_login_error("No account found.".into());
    h.app().set_login_account_missing(true);

    h.app().invoke_signin_google();

    assert!(h.app().get_show_sign_in(), "the panel stays open");
    assert!(h.app().get_login_loading(), "the attempt shows progress");
    assert_eq!(
        h.app().get_login_error().as_str(),
        "",
        "the old error clears"
    );
    assert!(!h.app().get_login_account_missing());

    h.emit(Event::LoginFailed {
        reason: "Authentication failed: User account not found.".into(),
    });
    assert!(h.app().get_show_sign_in(), "the failure shows on the panel");
    assert_eq!(visible_screens(&h), vec![Screen::SignIn]);
    assert_eq!(h.app().get_login_error().as_str(), "No account found.");
}

/// Documented gap, not an endorsement: desktop has no native Apple flow, so
/// the button sends an empty token the handler rejects as unsupported. Pinned
/// so that when a real flow lands, this test fails and gets updated.
#[test]
fn apple_signin_currently_sends_an_empty_token() {
    let mut h = Harness::new();
    h.app().invoke_signin_apple();

    let cmds = h.commands();
    let token = cmds.iter().find_map(|c| match c {
        Command::AuthApple { identity_token } => Some(identity_token.clone()),
        _ => None,
    });
    assert_eq!(
        token,
        Some(String::new()),
        "desktop has no native Apple flow yet; the handler reports unsupported"
    );
}

/// Logging out must return the user to a usable screen, not a blank one.
#[test]
fn logout_returns_to_a_visible_screen() {
    let mut h = Harness::new();
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    assert_eq!(visible_screens(&h), vec![Screen::App]);

    h.app().invoke_logout();

    assert!(!h.app().get_logged_in());
    h.assert_not_blank();
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::Logout)),
        "expected a Logout command, got {cmds:?}"
    );
}

/// A failed social link must clear the spinner and surface the reason.
/// Without this the user is left staring at a spinner that never resolves.
#[test]
fn failed_social_link_clears_the_spinner_and_shows_why() {
    let mut h = Harness::new();
    h.app().set_login_loading(true);

    h.emit(Event::SocialLinkFailed {
        reason: "provider rejected the token".into(),
    });

    assert!(
        !h.app().get_login_loading(),
        "a failed social link must stop the spinner"
    );
    assert_eq!(
        h.app().get_link_error().as_str(),
        "provider rejected the token"
    );
}

/// Same for email linking during onboarding.
#[test]
fn failed_email_link_shows_why() {
    let mut h = Harness::new();

    h.emit(Event::EmailLinkFailed {
        reason: "that email is already in use".into(),
    });

    assert_eq!(
        h.app().get_link_error().as_str(),
        "that email is already in use"
    );
}

/// A successful login must also stop the spinner and dismiss the panel.
#[test]
fn successful_login_clears_the_spinner_and_panel() {
    let mut h = Harness::new();
    h.app().set_login_loading(true);
    h.app().set_show_sign_in(true);

    h.emit(Event::LoggedIn {
        user: sample_user(),
    });

    assert!(!h.app().get_login_loading(), "the spinner must stop");
    assert!(
        !h.app().get_show_sign_in(),
        "the sign-in panel must dismiss"
    );
}

// ---------------------------------------------------------------------------
// Session restore
// ---------------------------------------------------------------------------

/// The full restore-succeeds path: spinner on, then the app.
#[test]
fn session_restore_success_ends_on_the_app_screen() {
    let mut h = Harness::new();

    h.emit(Event::Restoring);
    assert!(
        h.app().get_login_loading(),
        "restoring should show progress rather than a dead screen"
    );

    h.emit(Event::LoggedIn {
        user: sample_user(),
    });

    assert!(!h.app().get_login_loading());
    assert!(h.app().get_logged_in());
    assert_eq!(visible_screens(&h), vec![Screen::App]);
}

/// The full restore-fails path. `LoginFailed` with an *empty* reason is how
/// core signals "restore failed" as opposed to "these credentials are wrong",
/// and it must land somewhere the user can act, not on a blank screen.
#[test]
fn session_restore_failure_ends_somewhere_usable() {
    let mut h = Harness::new();
    h.app().set_onboarding_step(4);

    h.emit(Event::Restoring);
    h.emit(Event::LoginFailed {
        reason: String::new(),
    });

    assert!(!h.app().get_logged_in());
    assert_eq!(
        h.app().get_onboarding_step(),
        1,
        "a failed restore returns to crew selection"
    );
    h.assert_not_blank();
    assert_eq!(
        h.app().get_login_error().as_str(),
        "",
        "restore failing silently is not an error to show the user; they were \
         not trying to log in"
    );
}

/// A restore that fails must not leave the spinner running forever.
#[test]
fn failed_restore_stops_the_spinner() {
    let mut h = Harness::new();

    h.emit(Event::Restoring);
    assert!(h.app().get_login_loading());

    h.emit(Event::LoginFailed {
        reason: String::new(),
    });

    assert!(
        !h.app().get_login_loading(),
        "the restore spinner must stop when restore fails"
    );
}

// ── Sign-in entry points on step 1 (#67, #70) and a lost session (#71) ──────

const HAVE_ACCOUNT: &str = "I already have an account";
const OTHER_ACCOUNT: &str = "Use another account";
const RETURNING_SIGN_IN: &str = "Sign in";

fn with_device_account(h: &Harness) {
    h.settings().borrow_mut().device_id = Some("dev-abc".into());
}

/// The sign-in controls on screen, by label.
fn sign_in_controls(h: &Harness) -> Vec<&'static str> {
    [HAVE_ACCOUNT, OTHER_ACCOUNT, RETURNING_SIGN_IN]
        .into_iter()
        .flat_map(|label| std::iter::repeat_n(label, h.controls_labelled(label).len()))
        .collect()
}

/// ★ Regression (#67, R1): a fresh install has no device account, so step 1
/// offers "I already have an account", and nothing else to sign in with.
#[test]
fn fresh_install_step_one_offers_i_already_have_an_account() {
    let h = Harness::new();

    resume(h.ctx(), OnboardingState::PickCrew);

    assert_eq!(sign_in_controls(&h), vec![HAVE_ACCOUNT]);
}

/// ★ Regression: a device id on disk does not hide "I already have an
/// account". Before device auth answers, or when it does not find the
/// account, it is the only way into an existing account.
#[test]
fn a_device_account_still_offers_i_already_have_an_account() {
    let h = Harness::new();
    with_device_account(&h);

    resume(h.ctx(), OnboardingState::PickCrew);

    assert_eq!(sign_in_controls(&h), vec![HAVE_ACCOUNT]);
}

/// ★ Regression (#70, R5): a returning user who logged out sees the device
/// card's "Sign in" and "Use another account": two controls, two labels.
#[test]
fn a_returning_user_sees_the_device_card_and_another_account() {
    let mut h = Harness::new();
    with_device_account(&h);
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });

    h.app().invoke_logout();
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::DeviceAuth { .. })),
        "logout authenticates the device account for the device card, got {cmds:?}"
    );
    h.emit(Event::DeviceAuthed {
        user: sample_user(),
        created: false,
    });

    assert_eq!(h.app().get_onboarding_step(), 1);
    assert_eq!(sign_in_controls(&h), vec![OTHER_ACCOUNT, RETURNING_SIGN_IN]);

    h.click_label(OTHER_ACCOUNT);
    assert_eq!(visible_screens(&h), vec![Screen::SignIn]);
}

/// ★ Regression: the device card's "Sign in" opens the app on core's
/// session. A logout ends that session, so a card from an earlier device
/// auth must not show again until device auth answers.
#[test]
fn a_second_logout_shows_no_device_card_before_device_auth_answers() {
    let mut h = Harness::new();
    with_device_account(&h);
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.app().invoke_logout();
    h.emit(Event::DeviceAuthed {
        user: sample_user(),
        created: false,
    });
    h.click_label(RETURNING_SIGN_IN);
    assert!(h.app().get_logged_in());

    h.app().invoke_logout();

    assert_eq!(h.app().get_onboarding_step(), 1);
    assert_eq!(sign_in_controls(&h), vec![HAVE_ACCOUNT]);
}

/// ★ Regression (#67, R2): an unknown account gets a plain message and a way
/// to start as a new player, which returns to step 1 with no error left.
#[test]
fn an_unknown_account_gets_a_plain_message_and_a_way_back_to_step_one() {
    let mut h = Harness::new();
    resume(h.ctx(), OnboardingState::PickCrew);
    h.click_label(HAVE_ACCOUNT);
    assert_eq!(visible_screens(&h), vec![Screen::SignIn]);

    h.app()
        .invoke_login("nobody@example.test".into(), "whatever".into());
    h.emit(Event::LoginFailed {
        reason: "Authentication failed: User account not found.".into(),
    });

    assert_eq!(h.app().get_login_error().as_str(), "No account found.");
    assert!(h.app().get_login_account_missing());
    assert!(!h.app().get_login_loading());

    h.click_label("Start as a new player");

    assert_eq!(visible_screens(&h), vec![Screen::Onboarding]);
    assert_eq!(h.app().get_onboarding_step(), 1);
    assert_eq!(
        h.app().get_login_error().as_str(),
        "",
        "no sign-in error is left on step 1"
    );
    assert!(!h.app().get_login_account_missing());
}

/// ★ Regression (#67, R2): "Back" also clears the error, so the panel never
/// opens again on an old failure.
#[test]
fn leaving_the_sign_in_panel_clears_its_error() {
    let mut h = Harness::new();
    resume(h.ctx(), OnboardingState::PickCrew);
    h.click_label(HAVE_ACCOUNT);
    h.emit(Event::LoginFailed {
        reason: "Authentication failed: Invalid credentials.".into(),
    });
    assert_eq!(
        h.app().get_login_error().as_str(),
        "Wrong email or password."
    );

    h.click_label("Back");

    assert_eq!(visible_screens(&h), vec![Screen::Onboarding]);
    assert_eq!(h.app().get_login_error().as_str(), "");
}

/// ★ Regression (#71, R4): the saved session is lost, and device auth finds
/// the account that finished onboarding. The app opens directly.
#[test]
fn a_lost_session_with_a_device_account_opens_the_app() {
    let mut h = Harness::new();
    with_device_account(&h);
    resume(h.ctx(), OnboardingState::Done);
    let _ = h.commands();

    h.emit(Event::LoginFailed {
        reason: String::new(),
    });
    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::DeviceAuth { device_id } if device_id == "dev-abc")),
        "a failed restore tries the device account, got {cmds:?}"
    );
    assert_eq!(
        h.app().get_onboarding_step(),
        4,
        "no step 1 while device auth runs"
    );

    h.emit(Event::DeviceAuthed {
        user: sample_user(),
        created: false,
    });

    assert!(h.app().get_logged_in());
    assert_eq!(visible_screens(&h), vec![Screen::App]);
    assert_eq!(h.settings().borrow().onboarding_step, 4);
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::LoadMyCrews)),
        "the user's crews load, got {cmds:?}"
    );
}

/// A lost session whose device account turns out to be new (the server no
/// longer has it) starts onboarding as a new player.
#[test]
fn a_lost_session_with_a_new_device_account_goes_to_step_one() {
    let mut h = Harness::new();
    with_device_account(&h);
    resume(h.ctx(), OnboardingState::Done);

    h.emit(Event::LoginFailed {
        reason: String::new(),
    });
    h.emit(Event::DeviceAuthed {
        user: sample_user(),
        created: true,
    });

    assert!(!h.app().get_logged_in());
    assert_eq!(h.app().get_onboarding_step(), 1);
    h.assert_not_blank();
    // The old account is not on this server, but it may be on another:
    // step 1 still offers a way into it.
    assert_eq!(sign_in_controls(&h), vec![HAVE_ACCOUNT]);
}

/// Device auth after a lost session can fail too (the server is down). The
/// user lands on step 1, not on the blank restore wait, with no sign-in error:
/// nobody was signing in.
#[test]
fn a_lost_session_whose_device_auth_fails_goes_to_step_one() {
    let mut h = Harness::new();
    with_device_account(&h);
    resume(h.ctx(), OnboardingState::Done);

    h.emit(Event::LoginFailed {
        reason: String::new(),
    });
    h.emit(Event::LoginFailed {
        reason: "HTTP error: connection refused".into(),
    });

    assert_eq!(h.app().get_onboarding_step(), 1);
    assert_eq!(h.app().get_login_error().as_str(), "");
    h.assert_not_blank();
    assert_eq!(sign_in_controls(&h), vec![HAVE_ACCOUNT]);
}

/// ★ Regression (R3): step 3 has no skip. A restart at step 3 has no session
/// in core, so resuming it authenticates the device account; without that,
/// linking fails with "Not connected" and the user could not leave step 3.
#[test]
fn resuming_step_three_authenticates_the_device_account() {
    let mut h = Harness::new();
    with_device_account(&h);

    resume(h.ctx(), OnboardingState::LinkIdentity);

    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::DeviceAuth { device_id } if device_id == "dev-abc")),
        "step 3 needs a session to link an identity, got {cmds:?}"
    );
}

/// ★ Regression (R3): step 3 offers only ways to link an identity.
#[test]
fn step_three_has_no_skip() {
    let h = Harness::new();

    resume(h.ctx(), OnboardingState::LinkIdentity);

    assert!(!h.controls_labelled("Email + password").is_empty());
    assert!(
        h.controls_labelled("Skip for now").is_empty(),
        "step 3 requires linking one identity"
    );
}

// ── Entering a state is the same however you got there ─────────────────────
//
// `PickAvatar` has three entry paths and only one of them used to load the
// avatar grid and audio devices. The other two — resuming the persisted step
// after a restart, and the step indicator jumping back from step 3 — rendered a
// complete, correct-looking step 2 whose Continue button could never enable,
// because it requires a selected avatar and there were none to select.
//
// These assert on `ListAudioDevices`, which is observable as a Command. The
// avatar grid is covered by the reducer test in onboarding.rs, since loading it
// is a direct call rather than a command.

use crate::onboarding::{advance, resume, Input, OnboardingState};

/// END STREAM must change the screen on click, before the core answers. On
/// 2026-09-15 the core was blocked in a native stop and the button looked dead.
#[test]
fn end_stream_click_updates_ui_before_core_confirms() {
    let mut h = Harness::new();
    h.app().set_is_hosting(true);
    let _ = h.commands();

    h.app().invoke_stop_stream();

    assert!(
        !h.app().get_is_hosting(),
        "END STREAM must leave the hosting state without waiting for StreamEnded"
    );
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::StopStream)),
        "END STREAM must still send StopStream, got {cmds:?}"
    );
}

/// Hangup must change the screen on click, before the core answers.
#[test]
fn hangup_click_updates_ui_before_core_confirms() {
    let mut h = Harness::new();
    h.app().set_in_voice(true);
    let _ = h.commands();

    h.app().invoke_voice_toggle();

    assert!(
        !h.app().get_in_voice(),
        "hangup must leave the in-voice state without waiting for VoiceStateChanged"
    );
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::LeaveVoice)),
        "hangup must still send LeaveVoice, got {cmds:?}"
    );
}

fn listed_audio_devices(cmds: &[Command]) -> bool {
    cmds.iter().any(|c| matches!(c, Command::ListAudioDevices))
}

#[test]
fn entering_step_2_by_choosing_a_crew_loads_its_data() {
    let mut h = Harness::new();
    resume(h.ctx(), OnboardingState::PickCrew);
    let _ = h.commands();

    advance(h.ctx(), Input::CrewChosen);

    assert!(
        listed_audio_devices(&h.commands()),
        "the happy path must load step 2's audio devices"
    );
}

/// The avatar cards that the user can click on step 2, in grid order.
fn avatar_cards(h: &Harness) -> Vec<ElementHandle> {
    let cards = h.controls_labelled("Choose avatar");
    assert_eq!(cards.len(), 7, "step 2 shows seven avatar cards");
    cards
}

/// The deal-in does not swallow a click. Step 2 deals the cards in with an
/// opacity and scale animation. A click on a card that has not dealt in yet,
/// or that is in the middle of the deal, selects it.
#[test]
fn a_click_during_the_avatar_deal_in_selects_the_avatar() {
    let h = Harness::new();
    resume(h.ctx(), OnboardingState::PickAvatar);

    // No time has passed: no card has started its deal-in.
    assert!(!h.app().get_avatar_deal_0());
    avatar_cards(&h)[0].mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(
        h.app().get_selected_avatar(),
        0,
        "a card before its deal-in"
    );

    // Slot 1 deals in at 60 ms and animates for 350 ms.
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(100));
    assert!(h.app().get_avatar_deal_1());
    avatar_cards(&h)[1].mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(h.app().get_selected_avatar(), 1, "a card in its deal-in");
}

/// An image that arrives between pointer down and pointer up replaces the
/// card's placeholder. The click still selects the card.
#[test]
fn an_avatar_image_that_arrives_during_the_click_does_not_drop_it() {
    use slint::platform::{PointerEventButton, WindowEvent};

    let h = Harness::new();
    resume(h.ctx(), OnboardingState::PickAvatar);
    assert!(!h.app().get_avatar_loaded_0());

    let card = &avatar_cards(&h)[0];
    let (pos, size) = (card.absolute_position(), card.size());
    let center = slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
    let window = h.app().window();
    window.dispatch_event(WindowEvent::PointerMoved { position: center });
    window.dispatch_event(WindowEvent::PointerPressed {
        position: center,
        button: PointerEventButton::Left,
    });
    h.app().set_avatar_loaded_0(true);
    window.dispatch_event(WindowEvent::PointerReleased {
        position: center,
        button: PointerEventButton::Left,
    });

    assert_eq!(h.app().get_selected_avatar(), 0);
}

/// ★ Regression: a restart mid-onboarding used to resume into an empty step 2.
#[test]
fn resuming_into_step_2_loads_its_data() {
    let mut h = Harness::new();

    resume(h.ctx(), OnboardingState::PickAvatar);

    assert!(
        listed_audio_devices(&h.commands()),
        "resuming the persisted step must load the same data the click path \
         does; without it step 2 renders with no avatars and no devices, and \
         its Continue button can never enable"
    );
}

/// ★ Regression: the step indicator could walk a user back into an empty step 2
/// with no crash and no restart involved.
#[test]
fn navigating_back_into_step_2_loads_its_data() {
    let mut h = Harness::new();
    resume(h.ctx(), OnboardingState::LinkIdentity);
    let _ = h.commands();

    advance(h.ctx(), Input::GoBackTo(OnboardingState::PickAvatar));

    assert!(
        listed_audio_devices(&h.commands()),
        "stepping back into step 2 must reload its data"
    );
}

/// ★ Regression: a fresh install must ask for the crew list.
///
/// `Loading` renders nothing and only leaves on the discovery response. When
/// entry effects were introduced this nearly regressed, because the startup
/// state matched the property's default and the effect was skipped.
#[test]
fn a_fresh_install_requests_the_crew_list() {
    let mut h = Harness::new();

    resume(h.ctx(), OnboardingState::Loading);

    assert!(
        h.commands()
            .iter()
            .any(|c| matches!(c, Command::DiscoverCrews { .. })),
        "a fresh install sits in Loading, which renders nothing; without a \
         DiscoverCrews request it never leaves and the user sees a blank window"
    );
}

// ── A full crew must not be offered ────────────────────────────────────────
//
// `join_group` rejects a full crew with "Group is full", and onboarding only
// calls it at finalize — after the account has been created. A production user
// hit exactly that and was left on step 2 with a crew they could never join.
// The capacity was in the RPC response all along; it was dropped in the handler
// and absent from the Slint model, so the card had no way to know.

fn discover_crew_capacities(h: &Harness) -> Vec<(i32, i32)> {
    use slint::Model;
    h.app()
        .get_discover_crews()
        .iter()
        .map(|c| (c.member_count, c.max_members))
        .collect()
}

/// ★ Regression: capacity must survive the trip into the model.
#[test]
fn discovered_crews_carry_their_capacity() {
    let mut h = Harness::new();

    let mut crews = sample_crews(2);
    crews[0].member_count = 6;
    crews[0].max_members = 6; // full
    crews[1].member_count = 2;
    crews[1].max_members = 6; // room

    h.emit(Event::DiscoverCrewsLoaded {
        crews,
        cursor: None,
    });

    assert_eq!(
        discover_crew_capacities(&h),
        vec![(6, 6), (2, 6)],
        "max_members must reach the card; without it a full crew looks joinable \
         and only fails at finalize, after the account exists"
    );
}

/// Capacity of zero means "unknown", not "full" — an older server, or a crew
/// type without a cap, must not have every card greyed out.
#[test]
fn unknown_capacity_does_not_read_as_full() {
    let mut h = Harness::new();

    let mut crews = sample_crews(1);
    crews[0].member_count = 3;
    crews[0].max_members = 0;

    h.emit(Event::DiscoverCrewsLoaded {
        crews,
        cursor: None,
    });

    assert_eq!(
        discover_crew_capacities(&h),
        vec![(3, 0)],
        "an unknown capacity must pass through as 0 so the card treats it as \
         joinable rather than full"
    );
}

/// ★ Regression: onboarding's social buttons must *link*, never sign in.
///
/// By step 3 the user already has a device account. `AuthSteam` and friends
/// authenticate with `create=false`, so for someone linking that provider for
/// the first time — everyone, on this screen — Nakama answers "User account not
/// found" and the button simply fails. If the identity *did* already exist, it
/// was worse: the user was silently switched to that account, abandoning the
/// one and the crew they had just made.
///
/// Steam and Twitch had no link command at all, so they fell through to the
/// sign-in path. Google and Discord were already correct; this pins all four.
#[test]
fn onboarding_social_buttons_link_rather_than_sign_in() {
    type LinkCase = (&'static str, fn(&MainWindow), fn(&Command) -> bool);

    let cases: [LinkCase; 4] = [
        (
            "steam",
            |a| a.invoke_onboarding_auth_steam(),
            |c| matches!(c, Command::LinkSteam),
        ),
        (
            "twitch",
            |a| a.invoke_onboarding_auth_twitch(),
            |c| matches!(c, Command::LinkTwitch),
        ),
        (
            "google",
            |a| a.invoke_onboarding_auth_google(),
            |c| matches!(c, Command::LinkGoogle),
        ),
        (
            "discord",
            |a| a.invoke_onboarding_auth_discord(),
            |c| matches!(c, Command::LinkDiscord),
        ),
    ];

    for (label, press, is_expected_link) in cases {
        let mut h = Harness::new();
        press(h.app());
        let cmds = h.commands();

        assert!(
            cmds.iter().any(is_expected_link),
            "onboarding {label} must emit its Link command, got {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(
                c,
                Command::AuthSteam
                    | Command::AuthTwitch
                    | Command::AuthGoogle
                    | Command::AuthDiscord
            )),
            "onboarding {label} must not sign in: that abandons the account the \
             user just created, and fails outright for a first-time link"
        );
    }
}

fn waveform_peaks(model: &slint::ModelRc<f32>) -> Vec<f32> {
    (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect()
}

fn find_clip_card(cards: &slint::ModelRc<FeedCardData>) -> Option<FeedCardData> {
    for i in 0..cards.row_count() {
        if let Some(c) = cards.row_data(i) {
            if c.card_type == "clip" {
                return Some(c);
            }
        }
    }
    None
}

fn sample_waveform_b64() -> (String, Vec<f32>) {
    let bytes: Vec<u8> = (0..64).map(|i| (i * 4) as u8).collect();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let peaks = decode_clip_waveform(&b64);
    (b64, peaks)
}

/// ★ Regression: optimistic clip card carries decoded waveform peaks.
#[test]
fn clip_captured_populates_feed_card_waveform() {
    let mut h = Harness::new();
    let (b64, expected) = sample_waveform_b64();

    h.emit(Event::ClipCaptured {
        clip_id: "clip-opt".into(),
        path: "/tmp/clip.wav".into(),
        duration_seconds: 30.0,
        waveform: b64,
    });

    let card = find_clip_card(&h.app().get_feed_cards()).expect("clip card in feed");
    let peaks = waveform_peaks(&card.waveform);
    assert_eq!(peaks.len(), 64);
    assert_eq!(peaks, expected);
}

/// ★ Regression: crew feed clip entries decode base64 waveform metadata.
#[test]
fn feed_loaded_decodes_clip_waveform() {
    let mut h = Harness::new();
    let (b64, expected) = sample_waveform_b64();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    h.emit(Event::FeedLoaded {
        response: FeedResponse {
            crew_id: "crew-1".into(),
            sections: vec![FeedSection {
                id: "this_week".into(),
                entries: vec![FeedEntry {
                    id: "feed-clip-1".into(),
                    entry_type: "clip".into(),
                    role: "standard".into(),
                    size: "md".into(),
                    ts: now_ms,
                    data: serde_json::json!({
                        "waveform": b64,
                        "duration_seconds": 30.0,
                        "media_url": "/clips/test.wav",
                        "clipper_name": "alice",
                    }),
                }],
            }],
            joined: vec![],
        },
    });

    let card = find_clip_card(&h.app().get_feed_cards()).expect("clip card from feed");
    let peaks = waveform_peaks(&card.waveform);
    assert_eq!(peaks.len(), 64);
    assert_eq!(peaks, expected);
}

/// ★ Regression: pause/resume callbacks sync clip-paused UI state.
#[test]
fn pause_and_resume_clip_toggle_paused_property() {
    let h = Harness::new();

    h.app().invoke_pause_clip();
    assert!(h.app().get_clip_paused(), "pause-clip must set clip-paused");

    h.app().invoke_resume_clip();
    assert!(
        !h.app().get_clip_paused(),
        "resume-clip must clear clip-paused"
    );
}

/// ★ Regression: seek passes absolute milliseconds straight to the command.
/// The waveform computes position from its own duration input, so the value
/// must not be re-scaled against the (finish-reset) global clip-duration-ms.
#[test]
fn seek_clip_forwards_absolute_position_ms() {
    let mut h = Harness::new();
    // Global duration deliberately zeroed, as ClipPlaybackFinished leaves it:
    // seeks must stay correct when scrubbing an idle card anyway.
    h.app().set_clip_duration_ms(0);
    h.app().invoke_seek_clip_ms(15_000);

    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::SeekClip { position_ms: 15000 })),
        "expected SeekClip at 15000ms, got {cmds:?}"
    );
}

fn setup_crew_feed(h: &mut Harness, cards: Vec<FeedCardData>) {
    h.app().set_logged_in(true);
    h.app().set_onboarding_step(0);
    h.app().set_show_discover(false);
    h.app().set_active_crew_id("crew-1".into());
    h.app()
        .set_feed_cards(slint::ModelRc::new(slint::VecModel::from(cards)));
    h.pump();
}

/// ★ Regression: `alignment: start` on ClipCard's VerticalLayout disabled
/// `vertical-stretch`, leaving a void below the body in 204px side cells.
#[test]
fn clip_card_wave_band_fills_tall_side_cell() {
    let mut h = Harness::new();
    setup_crew_feed(
        &mut h,
        vec![
            FeedCardData {
                id: "hero".into(),
                card_type: "session-preview".into(),
                is_hero: true,
                title: "alice streamed Counter-Strike 2".into(),
                actor_name: "alice".into(),
                actor_initials: "al".into(),
                duration: "1h 2m".into(),
                duration_min: 62,
                timestamp: "2 days ago".into(),
                ..Default::default()
            },
            FeedCardData {
                id: "side-clip".into(),
                card_type: "clip".into(),
                title: "bobbi_twitch_1 clipped that".into(),
                actor_name: "bob".into(),
                actor_initials: "bo".into(),
                duration: "0:30".into(),
                duration_ms: 30_000,
                timestamp: "12m ago".into(),
                ..Default::default()
            },
        ],
    );

    let clip_cards: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "ClipCard").collect();
    assert_eq!(
        clip_cards.len(),
        1,
        "expected one ClipCard in the hero side stack"
    );

    let card = &clip_cards[0];
    let card_bottom = card.absolute_position().y + card.size().height;
    assert!(
        card.size().height >= 180.0,
        "side ClipCard should render tall (~204px), got {}",
        card.size().height
    );

    let mut wave_height = 0.0f32;
    let mut lowest_text_bottom = 0.0f32;
    card.visit_descendants(|el| {
        if el.type_name().as_deref() == Some("DotWaveform") {
            wave_height = wave_height.max(el.size().height);
        }
        if el.type_name().as_deref() == Some("Text") {
            let pos = el.absolute_position();
            let size = el.size();
            if size.height > 0.0 {
                lowest_text_bottom = lowest_text_bottom.max(pos.y + size.height);
            }
        }
        ControlFlow::<()>::Continue(())
    });

    assert!(
        wave_height >= 80.0,
        "wave band should absorb slack (height {wave_height} in {}px cell)",
        card.size().height
    );
    assert!(
        card_bottom - lowest_text_bottom <= 20.0,
        "body should sit near card bottom (gap {:.1}px)",
        card_bottom - lowest_text_bottom
    );
}

/// ★ Regression: full-width hero clip row uses the same stretch composition.
#[test]
fn hero_clip_card_wave_band_fills_full_width_row() {
    let mut h = Harness::new();
    setup_crew_feed(
        &mut h,
        vec![FeedCardData {
            id: "hero-clip".into(),
            card_type: "clip".into(),
            is_hero: true,
            title: "ostkatt clutch ace".into(),
            actor_name: "ostkatt".into(),
            actor_initials: "os".into(),
            duration: "0:30".into(),
            duration_ms: 30_000,
            timestamp: "2 hours ago".into(),
            ..Default::default()
        }],
    );

    let heroes: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "HeroClipCard").collect();
    assert_eq!(heroes.len(), 1, "expected one HeroClipCard");

    let card = &heroes[0];
    assert!(
        card.size().height >= 220.0,
        "hero clip row should be ~240px, got {}",
        card.size().height
    );

    let mut wave_height = 0.0f32;
    card.visit_descendants(|el| {
        if el.type_name().as_deref() == Some("DotWaveform") {
            wave_height = wave_height.max(el.size().height);
        }
        ControlFlow::<()>::Continue(())
    });

    assert!(
        wave_height >= 100.0,
        "hero wave band should grow in 240px row (height {wave_height})"
    );
}

/// ★ Regression: the hero play ring declared no explicit `x`, so Slint's
/// default (children narrower than their parent are horizontally centered)
/// dropped it mid-waveform instead of pinning it left of the band.
#[test]
fn hero_clip_play_ring_sits_left_of_wave_band() {
    let mut h = Harness::new();
    setup_crew_feed(
        &mut h,
        vec![FeedCardData {
            id: "hero-clip".into(),
            card_type: "clip".into(),
            is_hero: true,
            title: "ostkatt clutch ace".into(),
            actor_name: "ostkatt".into(),
            actor_initials: "os".into(),
            duration: "0:30".into(),
            duration_ms: 30_000,
            timestamp: "2 hours ago".into(),
            ..Default::default()
        }],
    );

    let heroes: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "HeroClipCard").collect();
    assert_eq!(heroes.len(), 1, "expected one HeroClipCard");

    // The ring must sit entirely LEFT of the wave band, like the mockup's
    // wave-row ([play button][wave]) — not floating over the dots.
    let card = &heroes[0];
    let mut ring_right_edge = 0.0f32;
    let mut wave_left_edge = f32::MAX;
    card.visit_descendants(|el| {
        match el.type_name().as_deref() {
            Some("ClipPlayRing") => {
                let pos = el.absolute_position();
                ring_right_edge = pos.x + el.size().width;
            }
            Some("DotWaveform") => {
                wave_left_edge = wave_left_edge.min(el.absolute_position().x);
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });

    assert!(
        ring_right_edge <= wave_left_edge,
        "play ring right edge ({ring_right_edge}) must be <= wave band left edge ({wave_left_edge})"
    );
}

// ---------------------------------------------------------------------------
// Feed — game session cards (GAME-SURFACING C1)
// ---------------------------------------------------------------------------

fn logged_in_app(h: &mut Harness) {
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.emit(Event::CrewsLoaded {
        crews: sample_crews(1),
    });
    assert_eq!(visible_screens(h), vec![Screen::App]);
}

fn zero_telemetry_game_session_feed() -> mello_core::crew_events::FeedResponse {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    mello_core::crew_events::FeedResponse {
        crew_id: "crew-0".into(),
        sections: vec![mello_core::crew_events::FeedSection {
            id: "this_week".into(),
            entries: vec![mello_core::crew_events::FeedEntry {
                id: "gs-t0-1".into(),
                entry_type: "session".into(),
                role: "quiet".into(),
                size: "sm".into(),
                ts,
                data: serde_json::json!({
                    "game_name": "Valorant",
                    "player_names": ["ostkatt"],
                    "player_ids": ["user-a"],
                    "duration_min": 252,
                    "wins": 0,
                    "losses": 0,
                    "draws": 0,
                }),
            }],
        }],
        joined: vec![],
    }
}

/// Zero-telemetry sessions must render the compact card shell, not empty W/L
/// stat slots.
#[test]
fn zero_telemetry_game_session_renders_compact_card_without_record() {
    let mut h = Harness::new();
    logged_in_app(&mut h);

    h.emit(Event::FeedLoaded {
        response: zero_telemetry_game_session_feed(),
    });

    let cards: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "GameSessionCard").collect();
    assert!(
        !cards.is_empty(),
        "expected a GameSessionCard in the feed for a zero-telemetry game session"
    );

    let compact: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "GameSessionCompactBody").collect();
    assert!(
        !compact.is_empty(),
        "zero-telemetry sessions must use the compact body, not the rich record layout"
    );

    let record: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "GameSessionRecordPanel").collect();
    assert!(
        record.is_empty(),
        "zero-telemetry sessions must not render W/L record stat slots"
    );
}

fn recent_games_feed() -> mello_core::crew_events::FeedResponse {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    mello_core::crew_events::FeedResponse {
        crew_id: "crew-0".into(),
        sections: vec![mello_core::crew_events::FeedSection {
            id: "this_week".into(),
            entries: vec![mello_core::crew_events::FeedEntry {
                id: "recent_games".into(),
                entry_type: "recent-games".into(),
                role: "standard".into(),
                size: "md".into(),
                ts,
                data: serde_json::json!({
                    "session_count": 14,
                    "total_min": 1860,
                    "games": [
                        { "game_id": "valorant", "game_name": "Valorant", "minutes": 600 },
                        { "game_id": "minecraft", "game_name": "Minecraft", "minutes": 240 },
                        { "game_id": "counter-strike-2", "game_name": "Counter-Strike 2", "minutes": 180 }
                    ]
                }),
            }],
        }],
        joined: vec![],
    }
}

/// The stats strip names a game from the catalogue. An id that nothing can
/// resolve shows a readable fallback, never the raw id.
#[test]
fn stats_strip_names_a_game_from_the_catalogue_and_never_shows_a_raw_id() {
    let stats = |id: &str| mello_core::crew_events::UserGameStats {
        game_id: id.into(),
        ..Default::default()
    };
    let mut h = Harness::new();
    logged_in_app(&mut h);

    h.emit(Event::UserGameStatsLoaded {
        games: vec![stats("counter-strike-2")],
    });
    assert_eq!(
        h.app().get_you_strip().game_name.as_str(),
        "Counter-Strike 2"
    );

    h.emit(Event::UserGameStatsLoaded {
        games: vec![stats("steam-4999999")],
    });
    let strip = h.app().get_you_strip();
    assert_eq!(strip.game_name.as_str(), "Unknown game");
    assert!(!strip.short_name.as_str().contains("4999999"));
}

/// The Crew play rollup is gone. Its session count and total time are the
/// header of the recent games card, which lists the games as tiles.
#[test]
fn recent_games_card_carries_header_and_tiles() {
    let mut h = Harness::new();
    logged_in_app(&mut h);

    h.emit(Event::FeedLoaded {
        response: recent_games_feed(),
    });

    let cards: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "RecentGamesCard").collect();
    assert!(!cards.is_empty(), "expected a RecentGamesCard in the feed");
    assert!(
        ElementHandle::find_by_element_type_name(h.app(), "GameRollupCard")
            .next()
            .is_none(),
        "the Crew play card must not exist"
    );

    let tiles: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "RecentGameTileCell").collect();
    assert_eq!(tiles.len(), 3, "one tile per game");

    let card = h.app().get_feed_cards().row_data(0).expect("feed card");
    assert_eq!(card.card_type.as_str(), "recent-games");
    assert_eq!(card.subtitle.as_str(), "14 sessions, 31 h this week");
}

fn recent_games_entry(games: serde_json::Value) -> FeedEntry {
    feed_entry(
        "recent_games",
        "recent-games",
        serde_json::json!({ "session_count": 0, "total_min": 0, "games": games }),
    )
}

/// A crew of two with a title that moved: the versus hero, then the recent
/// games card, built from `games`, each with the members who own it.
fn two_member_feed_with_games(games: serde_json::Value, pc_count: Option<i64>) -> FeedResponse {
    let moved = serde_json::json!({
        "title_id": "the_collector", "holder_user_id": "u2", "value": 14, "unit": "installed",
        "since_ms": 5_000, "moved_at_ms": 5_000, "previous_holder_user_id": "user-1",
    });
    let mut data = serde_json::json!({ "session_count": 1, "total_min": 85, "games": games });
    if let Some(n) = pc_count {
        data["pc_count"] = serde_json::json!(n);
    }
    feed_of(vec![
        h2h_entry(
            2,
            vec![
                h2h_title("the_carry", "user-1", serde_json::json!(4167.0), "h"),
                moved.clone(),
            ],
            vec![h2h_pair("u2", 1, 1)],
            Some(moved),
        ),
        feed_entry("recent_games", "recent-games", data),
    ])
}

/// Frame B2: two members own games, so the recent games card is a poster
/// grid of at most five tiles, two columns wide, under the versus hero. Each
/// poster carries the pips of the members who own the game.
#[test]
fn recent_games_is_a_poster_grid_when_two_members_own_games() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    let both = serde_json::json!(["user-1", "u2"]);
    h.emit(Event::FeedLoaded {
        response: two_member_feed_with_games(
            serde_json::json!([
                { "game_id": "steam-730", "game_name": "Counter-Strike 2", "minutes": 250_020, "member_ids": both },
                { "game_id": "steam-252950", "game_name": "Rocket League", "minutes": 4_000, "member_ids": both },
                { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 756, "member_ids": ["user-1"] },
                { "game_id": "valorant", "game_name": "Valorant", "minutes": 300, "member_ids": ["u2"] },
                { "game_id": "steam-1097150", "game_name": "Fall Guys", "minutes": 100, "member_ids": ["u2"] },
                { "game_id": "steam-1", "game_name": "Sixth Game", "minutes": 10, "member_ids": ["u2"] }
            ]),
            None,
        ),
    });

    let feed = h.app().get_feed_cards();
    let types: Vec<String> = (0..feed.row_count())
        .map(|i| feed.row_data(i).unwrap().card_type.to_string())
        .collect();
    assert_eq!(types[..2], ["head2head", "recent-games"], "{types:?}");
    let card = feed.row_data(1).unwrap();
    assert_eq!(card.recent_pc_count, 2, "two members own games");
    assert_eq!(
        card.subtitle.as_str(),
        "1 session, 1 h this week",
        "the week's line, not a note about the PCs"
    );
    assert_eq!(card.recent_games.row_count(), 5, "capped at five tiles");
    let first = card.recent_games.row_data(0).unwrap();
    assert_eq!(first.owners.row_count(), 2, "Bob and Jonas own it");
    assert_eq!(
        card.recent_games.row_data(2).unwrap().owners.row_count(),
        1,
        "one owner, one pip"
    );

    assert_eq!(
        count_of(&h, "RecentGamePoster"),
        5,
        "five posters in one row"
    );
    assert_eq!(count_of(&h, "RecentGameTileCell"), 0, "no list rows");
}

/// Frame A and the list: when the viewer is the only owner the card stays the
/// narrow list of three rows, even under a versus hero.
#[test]
fn recent_games_stays_a_list_when_the_viewer_is_the_only_owner() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    let me = serde_json::json!(["user-1"]);
    h.emit(Event::FeedLoaded {
        response: two_member_feed_with_games(
            serde_json::json!([
                { "game_id": "steam-730", "game_name": "Counter-Strike 2", "minutes": 250_020, "member_ids": me },
                { "game_id": "steam-252950", "game_name": "Rocket League", "minutes": 4_000, "member_ids": me },
                { "game_id": "steam-2300340", "game_name": "Night Stones", "minutes": 756, "member_ids": me },
                { "game_id": "valorant", "game_name": "Valorant", "minutes": 300, "member_ids": me }
            ]),
            None,
        ),
    });

    let card = h
        .app()
        .get_feed_cards()
        .iter()
        .find(|c| c.card_type.as_str() == "recent-games")
        .expect("a recent games card");
    assert_eq!(card.recent_pc_count, 1);
    assert_ne!(card.subtitle.as_str(), "From 2 PCs");
    assert_eq!(count_of(&h, "RecentGamePoster"), 0, "no poster grid");
    assert_eq!(
        count_of(&h, "RecentGameTileCell"),
        3,
        "the list shows three"
    );
}

/// A server that sends `pc_count` is believed, and one that does not still
/// gives the right shape from the tiles' owners.
#[test]
fn the_server_pc_count_wins_over_the_tile_owners() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: two_member_feed_with_games(
            serde_json::json!([
                { "game_id": "steam-730", "game_name": "Counter-Strike 2", "minutes": 250_020, "member_ids": ["user-1"] }
            ]),
            Some(2),
        ),
    });
    let card = h
        .app()
        .get_feed_cards()
        .iter()
        .find(|c| c.card_type.as_str() == "recent-games")
        .expect("a recent games card");
    assert_eq!(card.recent_pc_count, 2);
    assert_eq!(count_of(&h, "RecentGamePoster"), 1);
}

fn join_of(
    id: &str,
    name: &str,
    invited_by: &str,
    ago_ms: i64,
) -> mello_core::crew_events::FeedJoin {
    mello_core::crew_events::FeedJoin {
        user_id: id.into(),
        name: name.into(),
        invited_by: if invited_by.is_empty() {
            "".into()
        } else {
            // Nobody the roster knows: the name the event stored is shown.
            "u9".into()
        },
        invited_by_name: invited_by.into(),
        ts: now_ms() - ago_ms,
    }
}

fn feed_with_joins(joined: Vec<mello_core::crew_events::FeedJoin>) -> FeedResponse {
    let mut feed = two_member_feed_with_games(
        serde_json::json!([
            { "game_id": "steam-730", "game_name": "Counter-Strike 2", "minutes": 600, "member_ids": ["user-1", "u2"] }
        ]),
        None,
    );
    feed.joined = joined;
    feed
}

fn joined_cards(h: &Harness) -> Vec<FeedCardData> {
    h.app()
        .get_feed_cards()
        .iter()
        .filter(|c| c.card_type == "joined")
        .collect()
}

/// A join is a card in the grid, not a row above it. The joiner's octagon, "Jonas
/// joined" in Oxanium, who invited and when, and a "Say hi" action.
#[test]
fn a_joiner_is_a_card_with_the_inviter_the_time_and_say_hi() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_with_joins(vec![join_of("u2", "Jonas", "b0bben", 5 * 60_000)]),
    });

    let cards = joined_cards(&h);
    assert_eq!(cards.len(), 1);
    let c = &cards[0];
    assert_eq!(c.title.as_str(), "Jonas joined");
    assert_eq!(c.subtitle.as_str(), "invited by b0bben");
    assert!(
        c.timestamp.starts_with("Today ") || c.timestamp.starts_with("Yesterday "),
        "{}",
        c.timestamp
    );
    assert_eq!(count_of(&h, "JoinedCard"), 1);
    let texts = drawn_texts(&h);
    for want in ["Jonas joined", "invited by b0bben", "SAY HI"] {
        assert!(texts.iter().any(|t| t == want), "missing {want}: {texts:?}");
    }
    assert!(
        !texts.iter().any(|t| t.contains("joined, invited by")),
        "the old row is gone: {texts:?}"
    );
}

/// The card stands in the grid flow: after the first row, before the sessions.
#[test]
fn the_join_card_stands_after_the_first_row_and_before_the_session() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_with_joins(vec![join_of("u2", "Jonas", "", 5 * 60_000)]),
    });
    let types: Vec<String> = h
        .app()
        .get_feed_cards()
        .iter()
        .map(|c| c.card_type.to_string())
        .collect();
    let at = |t: &str| types.iter().position(|x| x == t);
    let joined = at("joined").expect("a join card");
    assert!(joined > at("head2head").unwrap(), "{types:?}");
    if let Some(session) = at("game-session") {
        assert!(joined < session, "before the sessions: {types:?}");
    }
}

/// "Say hi" sends a greeting to the crew chat at once. The click is the intent:
/// nothing is typed into the input first. The greeting mentions the new member
/// in the form the chat uses, and the card then reads "Said hi" and is off.
#[test]
fn say_hi_sends_a_greeting_that_mentions_the_joiner_and_a_second_click_does_nothing() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_with_joins(vec![join_of("u2", "Jonas", "", 5 * 60_000)]),
    });
    h.commands();
    assert_eq!(h.controls_labelled("Say hi to Jonas").len(), 1);
    assert_eq!(
        h.controls_labelled("Say hi to Jonas")[0].accessible_enabled(),
        Some(true)
    );

    h.click_label("Say hi to Jonas");
    let sent: Vec<_> = h
        .commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::SendMessage {
                content,
                mentions,
                reply_to,
            } => Some((content, mentions, reply_to)),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1, "one message at once: {sent:?}");
    let (content, mentions, reply_to) = &sent[0];
    assert!(content.contains("@Jonas"), "the mention form: {content}");
    assert_eq!(
        mentions,
        &vec![mello_core::chat::MentionRef {
            user_id: "u2".into(),
            name: "Jonas".into()
        }]
    );
    assert!(reply_to.is_none());
    assert!(
        !content.contains('!') && !content.to_lowercase().contains("team"),
        "{content}"
    );
    assert_eq!(h.app().get_chat_input_text().as_str(), "", "no draft");

    // The button reads "Said hi" and is off. A second click sends nothing.
    let said = h.controls_labelled("Said hi to Jonas");
    assert_eq!(said.len(), 1);
    assert_eq!(said[0].accessible_enabled(), Some(false));
    assert!(h.controls_labelled("Say hi to Jonas").is_empty());
    assert!(
        drawn_texts(&h).iter().any(|t| t == "SAID HI"),
        "{:?}",
        drawn_texts(&h)
    );
    said[0].mock_single_click(slint::platform::PointerEventButton::Left);
    h.pump();
    assert!(
        !h.commands()
            .iter()
            .any(|c| matches!(c, Command::SendMessage { .. })),
        "a second click does nothing"
    );
    // The invoke path is guarded too, not only the control.
    h.app()
        .global::<crate::ChatFocus>()
        .invoke_say_hi("joined-u2".into());
    assert!(h.commands().is_empty());
}

/// Once for each joiner, stored with the crew's local state: a restart does not
/// switch the button on again, and another joiner can still be greeted.
#[test]
fn said_hi_survives_a_restart_and_is_once_for_each_joiner() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 3);
    set_roster(
        &h,
        &[("user-1", "Test User"), ("u2", "Jonas"), ("u3", "Max")],
    );
    let feed = feed_with_joins(vec![
        join_of("u2", "Jonas", "", 5 * 60_000),
        join_of("u3", "Max", "", 10 * 60_000),
    ]);
    h.emit(Event::FeedLoaded {
        response: feed.clone(),
    });
    h.click_label("Say hi to Jonas");
    assert_eq!(
        h.settings().borrow().said_hi.get("crew-0"),
        Some(&vec!["u2".to_string()])
    );

    // The same feed again, as after a restart: Jonas stays off, Max is open.
    h.emit(Event::FeedLoaded {
        response: feed.clone(),
    });
    assert_eq!(h.controls_labelled("Said hi to Jonas").len(), 1);
    assert_eq!(h.controls_labelled("Say hi to Max").len(), 1);
    h.commands();
    h.click_label("Say hi to Max");
    let sent: Vec<_> = h
        .commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::SendMessage { mentions, .. } => Some(mentions),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0][0].user_id, "u3");
}

#[test]
fn the_join_cards_keep_three_newest_and_drop_old_and_own_joins() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 5);
    set_roster(
        &h,
        &[
            ("user-1", "Test User"),
            ("u2", "Jonas"),
            ("u3", "Max"),
            ("u4", "Sara"),
            ("u5", "Leo"),
        ],
    );
    let hour = 3_600_000;
    h.emit(Event::FeedLoaded {
        response: feed_with_joins(vec![
            join_of("u4", "Sara", "", 20 * hour),
            join_of("u2", "Jonas", "", hour),
            join_of("u5", "Leo", "", 30 * hour),
            join_of("user-1", "Test User", "", hour / 2),
            join_of("u3", "Max", "", 3 * hour),
            join_of("u6", "Gus", "", 10 * hour),
        ]),
    });
    let titles: Vec<String> = joined_cards(&h)
        .iter()
        .map(|c| c.title.to_string())
        .collect();
    assert_eq!(titles, ["Jonas joined", "Max joined", "Gus joined"]);
    assert_eq!(count_of(&h, "JoinedCard"), 3);
}

#[test]
fn no_join_in_the_last_day_gives_no_join_card() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_with_joins(vec![join_of("u2", "Jonas", "", 25 * 3_600_000)]),
    });
    assert!(joined_cards(&h).is_empty());
    assert_eq!(count_of(&h, "JoinedCard"), 0);
}

/// The tile shows hours in capitals, the launcher when the hours are unknown,
/// "TRACKED" only for a game the sensor catalogue knows, and a two-letter
/// monogram for a game with no art. Never the name clipped into a badge.
#[test]
fn recent_games_tiles_read_like_the_mockup() {
    let mut h = Harness::new();
    // A narrow window clips the right edge of the card, and clipped elements
    // are absent from queries.
    h.app()
        .window()
        .set_size(slint::PhysicalSize::new(1600, 1000));
    logged_in_app(&mut h);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![recent_games_entry(serde_json::json!([
            { "game_id": "steam-730", "game_name": "Counter-Strike 2", "minutes": 250_020, "launcher": "steam" },
            { "game_id": "epic-zzunknown", "game_name": "Zz Unknown Game", "minutes": 0, "launcher": "epic" },
            { "game_id": "steam-4999999", "game_name": "Night Stones", "minutes": 756 }
        ]))]),
    });
    let texts = drawn_texts(&h);
    for want in ["4,167 H", "EPIC", "12.6 H", "ZU", "NS", "RECENT GAMES"] {
        assert!(texts.iter().any(|t| t == want), "missing {want}: {texts:?}");
    }
    assert_eq!(
        texts.iter().filter(|t| t.as_str() == "TRACKED").count(),
        1,
        "only the catalogue game is tracked: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t == "From this PC"),
        "nothing is installed here: {texts:?}"
    );
}

/// The name in a recent games tile has the whole row: TRACKED sits on the
/// hours line. "Counter-Strike 2" elided to "Counter-Stri..." at 1440 px, in
/// the card beside the wide head2head card of a solo crew.
#[test]
fn a_recent_games_name_is_not_elided_at_1440_and_2000_px() {
    for width in [1440u32, 2000] {
        let mut h = Harness::new();
        h.app()
            .window()
            .set_size(slint::PhysicalSize::new(width, 900));
        app_with_crew_of(&mut h, 1);
        set_roster(&h, &[("user-1", "Test User")]);
        h.emit(Event::FeedLoaded {
            response: feed_of(vec![
                h2h_entry(
                    1,
                    vec![h2h_title("the_carry", "user-1", serde_json::json!(4167.0), "h")],
                    vec![],
                    None,
                ),
                recent_games_entry(serde_json::json!([
                    { "game_id": "steam-730", "game_name": "Counter-Strike 2", "minutes": 250_020, "launcher": "steam" }
                ])),
            ]),
        });
        let find = |label: &str| {
            ElementHandle::find_by_element_type_name(h.app(), "Text")
                .find(|e| e.accessible_label().is_some_and(|l| l == label))
        };
        let name = find("Counter-Strike 2").expect("the name is drawn");
        let tracked = find("TRACKED").expect("TRACKED is drawn");
        // The name takes the whole row beside the 76 px art and the 12 px gap.
        let cell = ElementHandle::find_by_element_type_name(h.app(), "RecentGameTileCell")
            .next()
            .expect("the tile row");
        let room = cell.size().width - 76.0 - 12.0;
        assert!(
            (name.size().width - room).abs() < 1.0,
            "{width}: the name has {} px of the {room} px beside the art",
            name.size().width
        );
        assert!(
            tracked.absolute_position().y > name.absolute_position().y,
            "{width}: TRACKED is under the name, not beside it"
        );
    }
}

/// The header of the recent games card is the week's line or nothing. It never
/// says where the games come from.
#[test]
fn recent_games_header_is_the_sessions_line_even_when_all_tiles_are_installed() {
    let mut h = Harness::new();
    logged_in_app(&mut h);
    h.emit(Event::InstalledGames {
        games: vec![
            installed_game("steam-730", "Counter-Strike 2", false),
            installed_game("epic-rl", "Rocket League", false),
        ],
    });
    let games = serde_json::json!([
        { "game_id": "counter-strike-2", "game_name": "Counter-Strike 2", "minutes": 600 },
        { "game_id": "epic-rl", "game_name": "Rocket League", "minutes": 0 }
    ]);
    let mut entry = recent_games_entry(games);
    entry.data["session_count"] = serde_json::json!(14);
    entry.data["total_min"] = serde_json::json!(1860);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![entry]),
    });
    assert_eq!(
        first_card(&h).subtitle.as_str(),
        "14 sessions, 31 h this week"
    );

    // A card with no sessions has no line, and still no note.
    let games = serde_json::json!([
        { "game_id": "counter-strike-2", "game_name": "Counter-Strike 2", "minutes": 600 }
    ]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![recent_games_entry(games)]),
    });
    assert_eq!(first_card(&h).subtitle.as_str(), "");
}

/// No card draws a note that explains it. The recap, the recent games card and
/// the quiet row state data, a label, a state or an action.
#[test]
fn no_card_draws_a_commentary_note() {
    let removed = [
        "From this PC",
        "From 2 PCs",
        "Monday",
        "MONDAY",
        "Head2head",
        "Now",
        "Today",
    ];
    let h = tall_recap_crew(serde_json::json!({
        "belts_moved": [belt_moved("the_carry", serde_json::json!(4201.0), "h")],
        "belts_held": belts_held(),
        "week_end": now_ms(),
    }));
    let texts = drawn_texts(&h);
    for want in ["WEEKLY RECAP", "BELTS MOVED", "BELTS HELD"] {
        assert!(texts.iter().any(|t| t == want), "missing {want}: {texts:?}");
    }
    for note in removed {
        assert!(!texts.iter().any(|t| t == note), "recap draws {note}");
    }

    for pc_count in [None, Some(2)] {
        let mut h = Harness::new();
        app_with_crew_of(&mut h, 2);
        set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
        let both = serde_json::json!(["user-1", "u2"]);
        h.emit(Event::FeedLoaded {
            response: two_member_feed_with_games(
                serde_json::json!([
                    { "game_id": "steam-730", "game_name": "Counter-Strike 2", "minutes": 600, "member_ids": both },
                    { "game_id": "steam-252950", "game_name": "Rocket League", "minutes": 400, "member_ids": both }
                ]),
                pc_count,
            ),
        });
        let texts = drawn_texts(&h);
        assert!(texts.iter().any(|t| t == "RECENT GAMES"), "{texts:?}");
        for note in removed {
            assert!(
                !texts.iter().any(|t| t == note),
                "recent games draws {note} for {pc_count:?}: {texts:?}"
            );
        }
    }
}

/// App with one crew of `members` people, as the crew list reports it.
fn app_with_crew_of(h: &mut Harness, members: i32) {
    // The default test window is 800 px wide, which leaves the feed too narrow
    // to lay three skeleton cells in view. Elements outside the feed's clip
    // are absent from queries.
    h.app()
        .window()
        .set_size(slint::PhysicalSize::new(1600, 1000));
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    let mut crews = sample_crews(1);
    crews[0].member_count = members;
    h.emit(Event::CrewsLoaded { crews });
    crate::converters::set_active_crew(h.app(), "crew-0");
    assert_eq!(visible_screens(h), vec![Screen::App]);
}

fn feed_of(entries: Vec<FeedEntry>) -> FeedResponse {
    FeedResponse {
        crew_id: "crew-0".into(),
        sections: vec![FeedSection {
            id: "this_week".into(),
            entries,
        }],
        joined: vec![],
    }
}

fn feed_entry(id: &str, entry_type: &str, data: serde_json::Value) -> FeedEntry {
    FeedEntry {
        id: id.into(),
        entry_type: entry_type.into(),
        role: "standard".into(),
        size: "md".into(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        data,
    }
}

fn skeleton_cells(h: &Harness) -> usize {
    ElementHandle::find_by_element_type_name(h.app(), "SkeletonCell").count()
}

/// A new empty crew shows exactly three skeletons (stream, voice clip, Monday
/// recap) and no card of any kind: no empty recap, no empty session.
#[test]
fn new_empty_crew_shows_three_skeletons_and_no_cards() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![]),
    });

    assert_eq!(skeleton_cells(&h), 3, "stream, voice clip, recap");
    assert!(h.app().get_feed_skeleton_stream());
    assert!(h.app().get_feed_skeleton_voice_clip());
    assert!(h.app().get_feed_skeleton_recap());
    assert_eq!(
        h.app().get_feed_cards().row_count(),
        0,
        "skeletons are not cards: nothing may reach the hero slot"
    );
    assert!(
        h.app().get_feed_invite_line(),
        "the invite line sits under the trio"
    );
    for card in ["RecapCard", "SessionCard", "SessionPreviewCard", "ClipCard"] {
        assert!(
            ElementHandle::find_by_element_type_name(h.app(), card)
                .next()
                .is_none(),
            "an empty crew must not render {card}"
        );
    }
}

/// A stream preview is the real stream card: its skeleton goes, two remain.
#[test]
fn stream_preview_removes_the_stream_skeleton() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![feed_entry(
            "evt-stream-1",
            "session-preview",
            serde_json::json!({
                "streamer_name": "ostkatt",
                "title": "Ranked",
                "duration_min": 30,
                "snapshot_urls": ["https://example.invalid/a.jpg"]
            }),
        )]),
    });

    assert_eq!(skeleton_cells(&h), 2, "voice clip and recap remain");
    assert!(!h.app().get_feed_skeleton_stream());
    assert!(h.app().get_feed_skeleton_voice_clip());
    assert!(h.app().get_feed_skeleton_recap());
}

/// Two members and a real card of another type keep the whole trio (frame B2).
#[test]
fn two_members_and_a_daily_recap_keep_every_skeleton() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![feed_entry(
            "daily_0",
            "daily-recap",
            serde_json::json!({ "day_start": 1, "total_min": 40, "session_count": 1 }),
        )]),
    });

    assert_eq!(skeleton_cells(&h), 3);
}

/// At two members a cell still goes when its own card type arrives.
#[test]
fn two_members_lose_a_skeleton_when_its_card_arrives() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![feed_entry(
            "evt-stream-1",
            "session-preview",
            serde_json::json!({
                "streamer_name": "ostkatt",
                "title": "Ranked",
                "duration_min": 30,
                "snapshot_urls": ["https://example.invalid/a.jpg"]
            }),
        )]),
    });

    assert_eq!(skeleton_cells(&h), 2, "voice clip and recap remain");
    assert!(!h.app().get_feed_skeleton_stream());
}

/// From three members on the crew is no longer new: no skeleton, with or
/// without a real card (frame C).
#[test]
fn three_members_hide_every_skeleton() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 3);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![]),
    });

    assert_eq!(skeleton_cells(&h), 0);
    assert!(!h.app().get_feed_invite_line());
}

/// A solo crew with one real card keeps the skeletons of the types it lacks.
#[test]
fn solo_crew_keeps_skeletons_for_missing_card_types() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![feed_entry(
            "daily_0",
            "daily-recap",
            serde_json::json!({ "day_start": 1, "total_min": 40, "session_count": 1 }),
        )]),
    });

    assert_eq!(skeleton_cells(&h), 3);
}

/// Hiding the invite takes the line away and leaves the trio.
#[test]
fn hiding_the_invite_removes_the_line_not_the_trio() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![]),
    });
    assert!(h.app().get_feed_invite_line());

    h.app().invoke_hide_invite_card();

    assert!(!h.app().get_feed_invite_line());
    assert_eq!(skeleton_cells(&h), 3);
}

// ---------------------------------------------------------------------------
// Head2head placement on the crew feed (plans/next-gen-feed.md section 3)
// ---------------------------------------------------------------------------

fn count_of(h: &Harness, type_name: &str) -> usize {
    ElementHandle::find_by_element_type_name(h.app(), type_name).count()
}

/// The crew list roster the head2head entry names by user id.
fn set_roster(h: &Harness, names: &[(&str, &str)]) {
    let members: Vec<crate::MemberData> = names
        .iter()
        .map(|(id, name)| crate::MemberData {
            id: (*id).into(),
            name: (*name).into(),
            initials: crate::converters::make_initials(name).into(),
            ..Default::default()
        })
        .collect();
    h.app()
        .set_members(slint::ModelRc::new(slint::VecModel::from(members)));
}

fn h2h_title(id: &str, holder: &str, value: serde_json::Value, unit: &str) -> serde_json::Value {
    serde_json::json!({
        "title_id": id, "holder_user_id": holder, "value": value, "unit": unit,
        "since_ms": 1_000,
    })
}

fn h2h_pair(opponent: &str, you: i32, them: i32) -> serde_json::Value {
    serde_json::json!({"a": "user-1", "b": opponent, "a_count": you, "b_count": them})
}

/// A pair with the values both members hold, as the backend sends them.
fn h2h_pair_values(
    opponent: &str,
    you: i32,
    them: i32,
    values: Vec<(&str, serde_json::Value, serde_json::Value)>,
) -> serde_json::Value {
    let values: Vec<_> = values
        .into_iter()
        .map(|(id, a, b)| serde_json::json!({"title_id": id, "a_value": a, "b_value": b}))
        .collect();
    serde_json::json!({"a": "user-1", "b": opponent, "a_count": you, "b_count": them, "values": values})
}

fn h2h_entry(
    members: i32,
    titles: Vec<serde_json::Value>,
    pairs: Vec<serde_json::Value>,
    moved: Option<serde_json::Value>,
) -> FeedEntry {
    let mut data = serde_json::json!({
        "crew_id": "crew-0", "member_count": members,
        "open_seats": (5 - members).max(0),
        "titles": titles, "pairs": pairs, "updated_at": 1_000,
    });
    if let Some(m) = moved {
        data["belt_moved_recent"] = m;
    }
    feed_entry("head2head", "head2head", data)
}

fn first_card(h: &Harness) -> FeedCardData {
    h.app().get_feed_cards().row_data(0).expect("a feed card")
}

fn invoke_invite_and_expect_code_request(h: &mut Harness, label: &str) {
    h.click_label(label);
    assert!(
        h.commands()
            .iter()
            .any(|c| matches!(c, Command::CreateInviteCode { crew_id } if crew_id == "crew-0")),
        "{label} asks for an invite code, the same action as the invite line"
    );
}

/// A crew of one: head2head takes the wide slot with the viewer's titles,
/// four open seats and the red invite inside the card. The trio stays under
/// it and the invite line gives way, so the surface has one red button.
#[test]
fn crew_of_one_shows_the_wide_card_with_four_seats_and_the_invite() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);
    set_roster(&h, &[("user-1", "Test User")]);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            1,
            vec![
                h2h_title("the_carry", "user-1", serde_json::json!(4167.0), "h"),
                h2h_title("night_owl", "user-1", serde_json::json!("03:14"), ""),
                h2h_title("the_collector", "user-1", serde_json::json!(5), "installed"),
                h2h_title("locked_in", "user-1", serde_json::json!(4.6), "h"),
            ],
            vec![],
            None,
        )]),
    });

    assert_eq!(count_of(&h, "Head2headCardWide"), 1);
    assert_eq!(count_of(&h, "Head2headCell"), 0);
    assert_eq!(count_of(&h, "VersusHero"), 0);
    assert_eq!(
        count_of(&h, "OpenSeat"),
        4,
        "the first five seats, one taken"
    );
    assert_eq!(count_of(&h, "TitleChip"), 4, "the viewer's four titles");
    assert_eq!(first_card(&h).head2head.variant.as_str(), "wide");
    assert_eq!(skeleton_cells(&h), 3, "the trio stays below");
    assert_eq!(h.controls_labelled("Invite your crew").len(), 1);
    assert!(
        h.controls_labelled("Share invite link").is_empty(),
        "one red button: the invite line gives way"
    );
    assert!(!h.app().get_feed_invite_line());
    invoke_invite_and_expect_code_request(&mut h, "Invite your crew");
}

/// A crew of one with no contested title: the server sends `own_values` and
/// the card draws the viewer's four first values as chips, four open seats
/// each labelled "OPEN", and the invite.
#[test]
fn solo_card_shows_four_own_value_chips_and_four_open_labels() {
    let mut h = Harness::new();
    // The chips of a new crew are sealed (day 0). This test reads the values,
    // so the four are opened first.
    h.settings().borrow_mut().sealed_opened.insert(
        "crew-0".into(),
        ["the_carry", "night_owl", "the_collector", "locked_in"]
            .iter()
            .map(|t| crate::sealed::own_key(t, "crew-0"))
            .collect(),
    );
    app_with_crew_of(&mut h, 1);
    set_roster(&h, &[("user-1", "Test User")]);

    let mut entry = h2h_entry(1, vec![], vec![], None);
    entry.data["own_values"] = serde_json::json!([
        {"title_id": "the_carry", "value": 4167.0, "unit": "h", "display": "4,167"},
        {"title_id": "night_owl", "value": "03:14", "unit": "", "display": "03:14"},
        {"title_id": "the_collector", "value": 5, "unit": "installed", "display": "5"},
        {"title_id": "locked_in", "value": 4.6, "unit": "h", "display": "4.6"},
        {"title_id": "sampler", "value": 3, "unit": "games", "display": "3"},
    ]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![entry]),
    });

    assert_eq!(count_of(&h, "Head2headCardWide"), 1);
    assert_eq!(count_of(&h, "TitleChip"), 4, "the first four own values");
    let texts = drawn_texts(&h);
    assert_eq!(
        texts.iter().filter(|t| t.as_str() == "OPEN").count(),
        4,
        "every seat is labelled"
    );
    for want in ["4,167", "03:14", "5", "4.6", "GO HEAD2HEAD WITH YOUR CREW"] {
        assert!(texts.iter().any(|t| t == want), "missing {want}: {texts:?}");
    }
    assert!(
        !texts.iter().any(|t| t == "3"),
        "the fifth value is not drawn"
    );
    assert_eq!(h.controls_labelled("Invite your crew").len(), 1);
}

/// Two members and no move in the last 24 h: the head2head card, two columns
/// wide, with the one pair and the seats that are left, not a hero.
#[test]
fn crew_of_two_without_a_move_shows_the_wide_card_with_one_pair_and_three_seats() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![
                h2h_title("the_carry", "user-1", serde_json::json!(4167.0), "h"),
                h2h_title("the_collector", "u2", serde_json::json!(14), "installed"),
            ],
            vec![h2h_pair("u2", 1, 1)],
            None,
        )]),
    });

    assert_eq!(count_of(&h, "Head2headCardPairs"), 1);
    assert_eq!(count_of(&h, "VersusHero"), 0);
    assert_eq!(count_of(&h, "Head2headCardWide"), 0);
    assert_eq!(count_of(&h, "PairRow"), 1);
    let card = first_card(&h);
    assert_eq!(card.head2head.variant.as_str(), "pairs");
    assert_eq!(card.head2head.open_seats, 3);
    // The card is wide: all three seats fit one line.
    assert_eq!(count_of(&h, "OpenSeat"), 3);
    assert_eq!(count_of(&h, "OpenSeatStrip"), 1);
    assert_two_columns_wide(&h, "Head2headCardPairs");
    assert_eq!(h.controls_labelled("Invite your crew").len(), 1);
}

const TEST_AVATAR_SVG: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 8 8'><rect width='8' height='8' fill='#c33'/></svg>";

/// The octagons of the head2head cell show the avatar the app loaded for the
/// user id, and initials for a user with none. An avatar that arrives after
/// the feed patches the card.
#[test]
fn head2head_octagons_show_the_loaded_avatar_and_initials_without_one() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 3);
    set_roster(
        &h,
        &[("user-1", "Test User"), ("u2", "Jonas"), ("u3", "Max")],
    );
    // Jonas loaded before the feed.
    h.emit(Event::UserAvatarLoaded {
        user_id: "u2".into(),
        data: TEST_AVATAR_SVG.into(),
    });
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            3,
            vec![h2h_title("the_carry", "u2", serde_json::json!(4167.0), "h")],
            vec![h2h_pair("u2", 1, 1), h2h_pair("u3", 0, 2)],
            None,
        )]),
    });
    let rows = first_card(&h).head2head.pairs;
    let row = |i: usize| rows.row_data(i).expect("pair row");
    let by_name = |n: &str| {
        (0..rows.row_count())
            .map(row)
            .find(|r| r.name == n)
            .unwrap()
    };
    assert!(
        by_name("Jonas").has_avatar,
        "the loaded avatar is on the row"
    );
    assert!(!by_name("Max").has_avatar, "no avatar, initials");
    assert!(!by_name("Max").you_has_avatar);

    // Max and the viewer arrive after the feed.
    h.emit(Event::UserAvatarLoaded {
        user_id: "u3".into(),
        data: TEST_AVATAR_SVG.into(),
    });
    h.emit(Event::UserAvatarLoaded {
        user_id: "user-1".into(),
        data: TEST_AVATAR_SVG.into(),
    });
    let card = first_card(&h);
    assert!(card.head2head.left_has_avatar, "the viewer octagon");
    let rows = card.head2head.pairs;
    for i in 0..rows.row_count() {
        let r = rows.row_data(i).unwrap();
        assert!(r.has_avatar && r.you_has_avatar, "{} wears images", r.name);
    }
}

/// A belt that moved in the last 24 h puts the versus card in the hero slot,
/// led by the moved title. The trio stays beside it (frame B2).
#[test]
fn a_recent_belt_move_puts_the_versus_card_in_the_hero_slot() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    let moved = serde_json::json!({
        "title_id": "the_collector", "holder_user_id": "u2", "value": 14, "unit": "installed",
        "since_ms": 5_000, "moved_at_ms": 5_000, "previous_holder_user_id": "user-1",
    });

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![
            feed_entry(
                "daily_0",
                "daily-recap",
                serde_json::json!({ "day_start": 1, "total_min": 40, "session_count": 1 }),
            ),
            h2h_entry(
                2,
                vec![
                    h2h_title("the_carry", "user-1", serde_json::json!(4167.0), "h"),
                    moved.clone(),
                ],
                vec![h2h_pair("u2", 1, 1)],
                Some(moved),
            ),
        ]),
    });

    assert_eq!(count_of(&h, "VersusHero"), 1);
    assert_eq!(count_of(&h, "Head2headCell"), 0);
    let hero = first_card(&h);
    assert_eq!(hero.card_type.as_str(), "head2head", "first in the feed");
    assert_eq!(hero.head2head.variant.as_str(), "versus");
    assert_eq!(hero.head2head.right_name.as_str(), "Jonas");
    // The hero has room for every open seat: three for a crew of two.
    assert_eq!(count_of(&h, "OpenSeat"), 3);
    assert!(!drawn_texts(&h).iter().any(|t| t.starts_with('+')));
    assert_eq!(
        hero.head2head.chips.row_data(0).unwrap().id.as_str(),
        "the_collector",
        "the moved title leads the chips"
    );
    // The button says INVITE on the versus card (frame B2), and keeps the
    // long name for a screen reader and for the harness.
    assert!(drawn_texts(&h).iter().any(|t| t == "INVITE"));
    assert_eq!(h.controls_labelled("Invite your crew").len(), 1);
}

/// A five member crew: u2 took the_carry from u3 two hours ago, u4 took
/// night_owl from u5 thirty hours ago. Neither belt is the viewer's, so no
/// hero: the cell carries them. The dot marks the pair with u2 and sorts it
/// first, though its tally is the widest.
fn feed_with_moved_belts() -> FeedResponse {
    let now = crate::head2head::now_ms();
    let moved = |id: &str, holder: &str, prev: &str, ago_h: i64| {
        let at = now - ago_h * 3_600_000;
        serde_json::json!({
            "title_id": id, "holder_user_id": holder, "value": 10, "unit": "h",
            "since_ms": at, "moved_at_ms": at, "previous_holder_user_id": prev,
        })
    };
    let recent = moved("the_carry", "u2", "u3", 2);
    let old = moved("night_owl", "u4", "u5", 30);
    feed_of(vec![h2h_entry(
        5,
        vec![recent.clone(), old],
        vec![
            h2h_pair("u3", 2, 2),
            h2h_pair("u4", 3, 3),
            h2h_pair("u5", 4, 4),
            h2h_pair("u2", 1, 6),
        ],
        Some(recent),
    )])
}

fn open_crew_feed_fresh(h: &mut Harness) {
    // No crew is active yet, as at sign in: the first select has no baseline.
    crate::converters::set_active_crew(h.app(), "");
    h.emit(Event::CrewJoined {
        crew_id: "crew-0".into(),
    });
}

fn dot_names(h: &Harness) -> Vec<(String, bool)> {
    let rows = first_card(h).head2head.pairs;
    (0..rows.row_count())
        .map(|i| {
            let r = rows.row_data(i).unwrap();
            (r.name.to_string(), r.moved_recent)
        })
        .collect()
}

/// A pair whose belt moved in the last 24 h wears the red dot and leads. A
/// belt that moved 30 h ago does not. Opening the feed again clears it.
#[test]
fn a_pair_row_with_a_belt_moved_today_wears_a_dot_until_the_feed_is_opened_again() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 5);
    set_roster(
        &h,
        &[
            ("user-1", "Test User"),
            ("u2", "Jonas"),
            ("u3", "Sara"),
            ("u4", "Max"),
            ("u5", "Leo"),
        ],
    );
    open_crew_feed_fresh(&mut h);
    h.emit(Event::FeedLoaded {
        response: feed_with_moved_belts(),
    });
    assert_eq!(
        first_card(&h).head2head.variant.as_str(),
        "pairs",
        "no hero"
    );
    // u2 holds the 2 h belt: only the pair with u2 wears the dot, and it
    // leads. Max holds a 30 h belt: no dot.
    let rows = dot_names(&h);
    assert_eq!(
        rows,
        vec![
            ("Jonas".to_string(), true),
            ("Sara".to_string(), false),
            ("Max".to_string(), false),
            ("Leo".to_string(), false),
        ]
    );

    // The viewer leaves and opens the feed again: read.
    h.emit(Event::CrewJoined {
        crew_id: "crew-1".into(),
    });
    h.emit(Event::CrewJoined {
        crew_id: "crew-0".into(),
    });
    h.emit(Event::FeedLoaded {
        response: feed_with_moved_belts(),
    });
    assert!(dot_names(&h).iter().all(|(_, dot)| !dot), "the dot cleared");
}

/// A live stream holds the hero and sends the first cards to the side column,
/// one column wide. Head2head is always two columns wide, so it never stands in
/// the column: it leads the grid beside the stream, wide, with all four seats.
#[test]
fn the_wide_card_stays_two_columns_wide_beside_a_live_stream() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);
    set_roster(&h, &[("user-1", "Test User")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            1,
            vec![h2h_title(
                "the_carry",
                "user-1",
                serde_json::json!(4167.0),
                "h",
            )],
            vec![],
            None,
        )]),
    });

    assert_eq!(count_of(&h, "Head2headCardWide"), 1, "no stream: wide slot");
    assert_eq!(count_of(&h, "OpenSeat"), 4);

    h.app()
        .set_active_stream_cards(slint::ModelRc::new(slint::VecModel::from(vec![
            crate::StreamCardData {
                host_id: "u2".into(),
                streamer_name: "alice".into(),
                can_watch: true,
                ..Default::default()
            },
        ])));

    // The card is two columns wide: it does not give way to a one-column cell.
    assert_eq!(count_of(&h, "Head2headCardWide"), 1, "still wide");
    assert_eq!(count_of(&h, "OpenSeat"), 4, "all four seats");
    assert_two_columns_wide(&h, "Head2headCardWide");
    assert_eq!(first_card(&h).head2head.variant.as_str(), "wide");
    assert_eq!(h.controls_labelled("Invite your crew").len(), 1);

    h.app()
        .set_active_stream_cards(slint::ModelRc::new(slint::VecModel::from(vec![])));
    assert_eq!(
        count_of(&h, "Head2headCardWide"),
        1,
        "stream ended: wide again"
    );
    assert_two_columns_wide(&h, "Head2headCardWide");
}

/// Five members: the wide card has no seats and no button, and the calm invite
/// card carries the one red button, wired to the share action.
#[test]
fn crew_of_five_shows_the_wide_card_without_seats_and_the_invite_card() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 5);
    set_roster(
        &h,
        &[
            ("user-1", "Test User"),
            ("u2", "Jonas"),
            ("u3", "Sara"),
            ("u4", "Max"),
            ("u5", "Leo"),
        ],
    );

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            5,
            vec![
                h2h_title("the_carry", "user-1", serde_json::json!(4167.0), "h"),
                h2h_title("the_collector", "u2", serde_json::json!(14), "installed"),
            ],
            vec![h2h_pair("u2", 1, 1), h2h_pair("u3", 1, 1)],
            None,
        )]),
    });

    assert_eq!(count_of(&h, "Head2headCardPairs"), 1);
    assert_two_columns_wide(&h, "Head2headCardPairs");
    assert_eq!(first_card(&h).head2head.variant.as_str(), "pairs");
    assert_eq!(first_card(&h).head2head.open_seats, 0);
    assert_eq!(count_of(&h, "OpenSeat"), 0, "no seats from five members");
    assert_eq!(count_of(&h, "InviteCard"), 1);
    assert_eq!(
        count_of(&h, "InviteButton"),
        1,
        "only the invite card has the button"
    );
    assert!(h.controls_labelled("Invite your crew").is_empty());
    assert_eq!(h.controls_labelled("Share invite link").len(), 1);
    let invite = (0..h.app().get_feed_cards().row_count())
        .filter_map(|i| h.app().get_feed_cards().row_data(i))
        .find(|c| c.card_type.as_str() == "invite")
        .expect("the invite card is in the feed");
    assert_eq!(invite.head2head.members.row_count(), 5);
    assert_eq!(invite.head2head.more_members, 0);
    invoke_invite_and_expect_code_request(&mut h, "Share invite link");
}

/// The feed can load before the member list. The invite card and the pair
/// names are built again when the list arrives, so they never keep the
/// short roster.
#[test]
fn a_feed_that_loads_before_the_member_list_is_built_again_with_the_roster() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 5);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            5,
            vec![h2h_title("the_carry", "u2", serde_json::json!(4167.0), "h")],
            vec![h2h_pair("u2", 1, 2)],
            None,
        )]),
    });
    let invite = |h: &Harness| {
        (0..h.app().get_feed_cards().row_count())
            .filter_map(|i| h.app().get_feed_cards().row_data(i))
            .find(|c| c.card_type.as_str() == "invite")
            .expect("the invite card")
    };
    assert_eq!(invite(&h).head2head.members.row_count(), 1, "viewer only");

    let member = |id: &str, name: &str| mello_core::crew_state::CrewMember {
        user_id: id.into(),
        username: name.into(),
        ..Default::default()
    };
    h.emit(Event::CrewStateLoaded {
        state: mello_core::crew_state::CrewState {
            crew_id: "crew-0".into(),
            name: "Crew".into(),
            members: Some(vec![
                member("user-1", "Test User"),
                member("u2", "Jonas"),
                member("u3", "Max"),
                member("u4", "Sara"),
                member("u5", "Leo"),
            ]),
            ..Default::default()
        },
    });
    assert_eq!(invite(&h).head2head.members.row_count(), 5);
    let cell = first_card(&h);
    assert_eq!(
        cell.head2head.pairs.row_data(0).unwrap().name.as_str(),
        "Jonas",
        "the pair row names the member, not Crew mate"
    );
}

/// More than eight members show eight octagons and a "+N".
#[test]
fn invite_card_shows_eight_members_then_a_count() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 11);
    let mut roster = vec![("user-1".to_string(), "Test User".to_string())];
    roster.extend((2..=11).map(|i| (format!("u{i}"), format!("Kim {i}"))));
    let roster_ref: Vec<(&str, &str)> = roster
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    set_roster(&h, &roster_ref);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(11, vec![], vec![], None)]),
    });

    assert_eq!(
        count_of(&h, "Head2headCell"),
        0,
        "no data, no head2head cell"
    );
    let card = first_card(&h);
    assert_eq!(card.card_type.as_str(), "invite");
    assert_eq!(card.head2head.members.row_count(), 8);
    assert_eq!(card.head2head.more_members, 3);
}

/// The chip on a pair row is the contested title with the largest gap: the
/// 4,171 h Carry, not the 4.6 h Locked In.
#[test]
fn the_deciding_chip_is_the_title_with_the_largest_gap() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![
                h2h_title("locked_in", "user-1", serde_json::json!(4.6), "h"),
                h2h_title("the_carry", "u2", serde_json::json!(4171.0), "h"),
            ],
            vec![h2h_pair_values(
                "u2",
                1,
                1,
                vec![
                    ("locked_in", serde_json::json!(4.6), serde_json::json!(4.4)),
                    (
                        "the_carry",
                        serde_json::json!(4.6),
                        serde_json::json!(4171.0),
                    ),
                ],
            )],
            None,
        )]),
    });

    let row = first_card(&h)
        .head2head
        .pairs
        .row_data(0)
        .expect("a pair row");
    assert!(row.has_chip);
    assert_eq!(row.chip.title.as_str(), "The Carry");
    assert_eq!(row.chip.value.as_str(), "4,171");
    assert_eq!(row.chip.holder_id.as_str(), "u2");
    assert_eq!(count_of(&h, "TitleChip"), 1, "one chip on the one row");
}

/// Two pairs that share the same widest gap do not wear the same chip: the
/// lower row takes the next contested title.
#[test]
fn a_deciding_chip_already_used_above_gives_way_in_the_next_row() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 5);
    set_roster(
        &h,
        &[
            ("user-1", "Test User"),
            ("u2", "Jonas"),
            ("u3", "Max"),
            ("u4", "Sara"),
            ("u5", "Leo"),
        ],
    );
    let values = || {
        vec![
            ("the_collector", serde_json::json!(6), serde_json::json!(1)),
            ("locked_in", serde_json::json!(9.0), serde_json::json!(6.0)),
        ]
    };
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            5,
            vec![
                h2h_title("the_collector", "user-1", serde_json::json!(6), "installed"),
                h2h_title("locked_in", "user-1", serde_json::json!(9.0), "h"),
            ],
            vec![
                h2h_pair_values("u2", 1, 1, values()),
                h2h_pair_values("u3", 1, 1, values()),
            ],
            None,
        )]),
    });
    let pairs = first_card(&h).head2head.pairs;
    assert_eq!(pairs.row_count(), 2);
    assert_eq!(
        pairs.row_data(0).expect("row").chip.title.as_str(),
        "The Collector"
    );
    assert_eq!(
        pairs.row_data(1).expect("row").chip.title.as_str(),
        "Locked In"
    );
}

/// The text of every `Text` the window draws.
fn drawn_texts(h: &Harness) -> Vec<String> {
    ElementHandle::find_by_element_type_name(h.app(), "Text")
        .filter_map(|e| e.accessible_label())
        .map(|s| s.to_string())
        .collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis() as i64
}

/// A title the holder took `days` whole days ago, plus an hour of margin so a
/// slow test run cannot cross the day line.
fn h2h_title_held(
    id: &str,
    holder: &str,
    value: serde_json::Value,
    unit: &str,
    days: i64,
) -> serde_json::Value {
    serde_json::json!({
        "title_id": id, "holder_user_id": holder, "value": value, "unit": unit,
        "since_ms": now_ms() - days * 86_400_000 - 3_600_000,
    })
}

/// The held label under each chip reads "held 9 days", "held 1 day", and is
/// absent under one day. Checked in the data and in what the card draws.
#[test]
fn held_days_label_sits_under_the_chip_in_the_wide_card() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);
    set_roster(&h, &[("user-1", "Test User")]);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            1,
            vec![
                h2h_title_held("the_carry", "user-1", serde_json::json!(4167.0), "h", 9),
                h2h_title_held("night_owl", "user-1", serde_json::json!("03:14"), "", 1),
                h2h_title_held(
                    "the_collector",
                    "user-1",
                    serde_json::json!(5),
                    "installed",
                    0,
                ),
            ],
            vec![],
            None,
        )]),
    });

    let chips = first_card(&h).head2head.chips;
    let labels: Vec<String> = (0..chips.row_count())
        .map(|i| chips.row_data(i).unwrap().held_label.to_string())
        .collect();
    assert_eq!(labels, ["held 9 days", "held 1 day", ""]);
    assert_eq!(count_of(&h, "HeldChip"), 3);
    let drawn = drawn_texts(&h);
    assert!(drawn.iter().any(|t| t == "held 9 days"), "{drawn:?}");
    assert!(drawn.iter().any(|t| t == "held 1 day"), "{drawn:?}");
    assert_eq!(
        drawn.iter().filter(|t| t.starts_with("held ")).count(),
        2,
        "nothing under one day: {drawn:?}"
    );
}

/// The same label sits under the chip of a pair row in the cell.
#[test]
fn held_days_label_sits_under_the_chip_in_a_pair_row() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);

    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![h2h_title_held(
                "the_carry",
                "u2",
                serde_json::json!(4171.0),
                "h",
                9,
            )],
            vec![h2h_pair_values(
                "u2",
                0,
                1,
                vec![(
                    "the_carry",
                    serde_json::json!(4.6),
                    serde_json::json!(4171.0),
                )],
            )],
            None,
        )]),
    });

    let row = first_card(&h).head2head.pairs.row_data(0).unwrap();
    assert_eq!(row.chip.held_label.as_str(), "held 9 days");
    assert!(drawn_texts(&h).iter().any(|t| t == "held 9 days"));
}

/// ★ Regression: discover had no way back. `back-requested` was declared on
/// DiscoverPanel and wired in main.slint, but nothing emitted it, and the
/// panel had no header to put a control in.
///
/// The control shows the name of the crew the user came from, so that name
/// has to follow the active crew id. Setting the id alone left it stale, and
/// a stale name means the control offers to return to the wrong crew.
#[test]
fn active_crew_name_follows_the_active_crew() {
    use crate::converters::set_active_crew;
    use crate::CrewData;

    let h = Harness::new();
    h.app()
        .set_crews(slint::ModelRc::new(slint::VecModel::from(vec![
            CrewData {
                id: "crew-1".into(),
                name: "M3LLO CREW".into(),
                ..Default::default()
            },
            CrewData {
                id: "crew-2".into(),
                name: "Night Stones".into(),
                ..Default::default()
            },
        ])));

    set_active_crew(h.app(), "crew-2");
    assert_eq!(h.app().get_active_crew_id(), "crew-2");
    assert_eq!(
        h.app().get_active_crew_name(),
        "Night Stones",
        "the back control names the crew the user came from"
    );

    set_active_crew(h.app(), "crew-1");
    assert_eq!(h.app().get_active_crew_name(), "M3LLO CREW");

    // Leaving the last crew leaves nowhere to go back to, and the control
    // hides on an empty name.
    set_active_crew(h.app(), "");
    assert_eq!(h.app().get_active_crew_id(), "");
    assert_eq!(h.app().get_active_crew_name(), "");
}

/// ★ Regression: the crew menu is a PopupWindow. The e2e driver, like a
/// screen reader, presses its items through the default accessibility action,
/// because a pointer click by element position misses inside a popup. The
/// items answered only a click, so "Add channel" did nothing.
#[test]
fn crew_menu_add_channel_answers_the_accessibility_action() {
    use crate::CrewData;

    let mut h = Harness::new();
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.app()
        .set_crews(slint::ModelRc::new(slint::VecModel::from(vec![CrewData {
            id: "crew-1".into(),
            name: "M3LLO CREW".into(),
            ..Default::default()
        }])));
    crate::converters::set_active_crew(h.app(), "crew-1");
    h.app().set_can_manage_channels(true);
    h.pump();

    h.click_label("Crew menu");
    let items = h.controls_labelled("Add channel");
    assert_eq!(
        items.len(),
        1,
        "the open crew menu shows one \"Add channel\""
    );
    items[0].invoke_accessible_default_action();
    h.pump();

    assert!(
        h.app().get_crew_settings_open(),
        "the accessibility action on \"Add channel\" opens crew settings"
    );
    assert_eq!(
        h.app().get_crew_settings_active_tab(),
        1,
        "crew settings open on the channels tab"
    );
    assert!(
        h.controls_labelled("Leave crew").is_empty(),
        "the action closes the crew menu, as a click on an item does"
    );
}

/// Two crews, "crew-1" active with the online member "u2", and the crew menu
/// open on the active crew card.
fn crew_menu_open() -> Harness {
    use crate::{CrewData, MemberData};

    let mut h = Harness::new();
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.app()
        .set_crews(slint::ModelRc::new(slint::VecModel::from(vec![
            CrewData {
                id: "crew-1".into(),
                name: "M3LLO CREW".into(),
                ..Default::default()
            },
            CrewData {
                id: "crew-2".into(),
                name: "Night Stones".into(),
                ..Default::default()
            },
        ])));
    h.app()
        .set_members(slint::ModelRc::new(slint::VecModel::from(vec![
            MemberData {
                id: "u2".into(),
                name: "kim".into(),
                initials: "KI".into(),
                online: true,
                ..Default::default()
            },
        ])));
    crate::converters::set_active_crew(h.app(), "crew-1");
    h.app().set_can_manage_channels(true);
    h.pump();

    h.click_label("Crew menu");
    assert_eq!(
        h.controls_labelled("Add channel").len(),
        1,
        "the crew menu is open"
    );
    h
}

fn crew_row(h: &Harness, id: &str) -> crate::CrewData {
    let crews = h.app().get_crews();
    (0..crews.row_count())
        .filter_map(|i| crews.row_data(i))
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no crew {id:?} in the crew list"))
}

fn assert_crew_menu_open(h: &Harness, after: &str) {
    assert_eq!(
        h.controls_labelled("Add channel").len(),
        1,
        "the crew menu is still open after {after}"
    );
}

/// ★ Regression (#105): the server sends a sidebar batch every 30 s. The
/// handler replaced the crews model, Slint rebuilt the active crew card, and
/// the crew menu on it closed under the mouse.
#[test]
fn crew_menu_survives_a_sidebar_update() {
    let mut h = crew_menu_open();

    h.emit(Event::SidebarUpdated {
        crews: vec![mello_core::crew_state::CrewSidebarState {
            crew_id: "crew-1".into(),
            name: "M3LLO CREW".into(),
            counts: mello_core::crew_state::CrewCounts {
                online: 3,
                total: 5,
            },
            ..Default::default()
        }],
    });

    assert_eq!(crew_row(&h, "crew-1").online_count, 3, "the update applies");
    assert_crew_menu_open(&h, "a sidebar update");
}

/// ★ Regression (#105): every change in who speaks updates the active crew
/// card. It replaced the crews model too, so in voice the menu closed at once.
#[test]
fn crew_menu_survives_a_speaking_change() {
    let mut h = crew_menu_open();

    h.emit(Event::VoiceActivity {
        member_id: "u2".into(),
        speaking: true,
    });

    assert!(crew_row(&h, "crew-1").v0_speaking, "the update applies");
    assert_crew_menu_open(&h, "a speaking change");
}

/// ★ Regression (#105): message previews and catch-up text update the crew
/// list as well.
#[test]
fn crew_menu_survives_preview_and_catchup_updates() {
    let mut h = crew_menu_open();

    h.emit(Event::MessagePreviewUpdated {
        crew_id: "crew-1".into(),
        messages: vec![mello_core::crew_state::MessagePreview {
            username: "kim".into(),
            preview: "gg".into(),
            ..Default::default()
        }],
    });
    assert_eq!(crew_row(&h, "crew-1").m0_text, "gg", "the preview applies");
    assert_crew_menu_open(&h, "a message preview");

    h.emit(Event::CatchupLoaded {
        response: mello_core::crew_events::CatchupResponse {
            crew_id: "crew-2".into(),
            catchup_text: "2 clips".into(),
            event_count: 2,
            top_events: vec![],
            has_events: true,
        },
    });
    assert_eq!(
        crew_row(&h, "crew-2").catchup_text,
        "2 clips",
        "the catch-up applies"
    );
    assert_crew_menu_open(&h, "a catch-up");
}

/// ★ Regression (#105): another crew leaves the list. The rows change, and
/// the active crew card and its menu stay.
#[test]
fn crew_menu_survives_another_crew_removed() {
    let mut h = crew_menu_open();

    h.emit(Event::CrewDeleted {
        crew_id: "crew-2".into(),
    });

    assert_eq!(h.app().get_crews().row_count(), 1, "crew-2 is gone");
    assert_crew_menu_open(&h, "another crew was removed");
}

/// `sync_crews` keeps the one crews model and the order it is given: it
/// changes, inserts and removes rows, and a new order replaces the rows.
#[test]
fn sync_crews_updates_the_crew_list_in_place() {
    use crate::converters::sync_crews;
    use crate::CrewData;

    let crew = |id: &str, online: i32| CrewData {
        id: id.into(),
        online_count: online,
        ..Default::default()
    };
    let ids = |h: &Harness| -> Vec<(String, i32)> {
        let crews = h.app().get_crews();
        (0..crews.row_count())
            .filter_map(|i| crews.row_data(i))
            .map(|c| (c.id.to_string(), c.online_count))
            .collect()
    };
    let row = |id: &str, online: i32| (id.to_string(), online);

    let h = Harness::new();
    // The default model is not a VecModel: the first sync replaces it.
    sync_crews(h.app(), vec![crew("a", 0), crew("b", 0)]);
    let model = h.app().get_crews();
    assert_eq!(ids(&h), vec![row("a", 0), row("b", 0)]);

    sync_crews(h.app(), vec![crew("a", 2), crew("b", 0)]);
    assert_eq!(ids(&h), vec![row("a", 2), row("b", 0)], "a row changes");

    sync_crews(h.app(), vec![crew("a", 2), crew("c", 1), crew("b", 0)]);
    assert_eq!(
        ids(&h),
        vec![row("a", 2), row("c", 1), row("b", 0)],
        "a new crew goes in at its place"
    );

    sync_crews(h.app(), vec![crew("c", 1), crew("b", 0)]);
    assert_eq!(
        ids(&h),
        vec![row("c", 1), row("b", 0)],
        "a gone crew leaves"
    );
    assert!(
        h.app().get_crews() == model,
        "changes, inserts and removals keep the same model"
    );

    sync_crews(h.app(), vec![crew("b", 0), crew("c", 1)]);
    assert_eq!(
        ids(&h),
        vec![row("b", 0), row("c", 1)],
        "a new order applies"
    );
}

/// The active crew card follows the active crew id after in-place updates.
#[test]
fn active_crew_card_follows_the_active_crew_after_an_update() {
    let mut h = crew_menu_open();
    h.emit(Event::SidebarUpdated {
        crews: vec![mello_core::crew_state::CrewSidebarState {
            crew_id: "crew-2".into(),
            name: "Night Stones".into(),
            ..Default::default()
        }],
    });

    crate::converters::set_active_crew(h.app(), "crew-2");
    h.pump();

    assert!(
        h.controls_labelled("Add channel").is_empty(),
        "the old card closes with its menu"
    );
    let active: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "ActiveCrewCard").collect();
    let compact: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "CompactCrewCard").collect();
    assert_eq!(active.len(), 1, "one active crew card");
    assert_eq!(compact.len(), 1, "one compact crew card");
    h.click_label("Crew menu");
    assert_crew_menu_open(&h, "the switch to crew-2");
}

/// The quality pills in the STREAM menu and the window picker write one
/// property. The quick path — STREAM with a game detected — used to send a
/// hardcoded Medium, so the pills were a lie on the path most people take.
#[test]
fn the_quick_stream_path_uses_the_chosen_quality() {
    let mut h = Harness::new();
    h.app().set_logged_in(true);
    h.app().set_active_crew_id("crew-1".into());
    h.app().set_game_name("Counter-Strike 2".into());
    h.ctx()
        .fg_monitor
        .borrow_mut()
        .set_game_active(true, Some(4242));

    // Ultra, not the old hardcoded Medium.
    h.app().set_stream_preset(0);
    h.pump();
    h.app().invoke_stream_requested();
    h.pump();

    let cmds = h.commands();
    let preset = cmds.iter().find_map(|c| match c {
        Command::StartStream { preset, pid, .. } if *pid == Some(4242) => Some(*preset),
        _ => None,
    });
    assert_eq!(
        preset,
        Some(0),
        "STREAM must send the preset the pills chose, got {cmds:?}"
    );
}

/// One capture source for the hook identity tests.
fn game_source(pid: u32, exe: &str, igdb_id: u32) -> mello_core::events::CaptureSource {
    mello_core::events::CaptureSource {
        id: format!("game-{pid}"),
        name: exe.into(),
        mode: "process".into(),
        monitor_index: None,
        hwnd: None,
        pid: Some(pid),
        exe: exe.into(),
        igdb_id,
        is_fullscreen: true,
        resolution: String::new(),
    }
}

/// Two running games: Counter-Strike 2 (pid 5151) is listed first, The
/// Witcher 3 (pid 4242) second. A lookup that takes the first row, or any
/// row but the pid's own, sends the wrong identity.
fn two_games_listed(h: &mut Harness) {
    h.emit(Event::CaptureSourcesListed {
        monitors: vec![],
        games: vec![
            game_source(5151, "cs2.exe", 242408),
            game_source(4242, "witcher3.exe", 1942),
        ],
        windows: vec![],
    });
}

/// The hook policy invariant (game-identity plan §3): the identity sent with
/// a stream start belongs to the process that is captured. The quick path
/// starts pid 4242, so it must send 4242's id, not the id of another row.
#[test]
fn the_quick_stream_path_sends_the_identity_of_the_captured_pid() {
    let mut h = Harness::new();
    h.app().set_logged_in(true);
    h.app().set_active_crew_id("crew-1".into());
    h.app().set_game_name("The Witcher 3".into());
    two_games_listed(&mut h);
    h.ctx()
        .fg_monitor
        .borrow_mut()
        .set_game_active(true, Some(4242));
    h.pump();
    h.app().invoke_stream_requested();
    h.pump();

    let cmds = h.commands();
    let sent = cmds.iter().find_map(|c| match c {
        Command::StartStream {
            pid, exe, igdb_id, ..
        } => Some((*pid, exe.clone(), *igdb_id)),
        _ => None,
    });
    assert_eq!(
        sent,
        Some((Some(4242), "witcher3.exe".to_string(), 1942)),
        "the start must carry the identity of pid 4242, got {cmds:?}"
    );
}

/// The picker path carries the identity of the picked row from the UI
/// callback to the emitted command, and an unknown process sends 0.
#[test]
fn the_picker_sends_the_identity_of_the_picked_source() {
    let mut h = Harness::new();
    h.app().set_logged_in(true);
    h.app().set_active_crew_id("crew-1".into());
    h.emit(Event::CaptureSourcesListed {
        monitors: vec![],
        games: vec![
            game_source(5151, "cs2.exe", 242408),
            game_source(4242, "witcher3.exe", 1942),
            game_source(7777, "indie.exe", 0),
        ],
        windows: vec![],
    });

    let identity_of = |h: &mut Harness, pid: u32| {
        h.app()
            .invoke_start_stream(format!("game-{pid}").into(), "process".into(), 2);
        h.pump();
        h.commands().iter().rev().find_map(|c| match c {
            Command::StartStream {
                pid: Some(p),
                igdb_id,
                ..
            } if *p == pid => Some(*igdb_id),
            _ => None,
        })
    };
    assert_eq!(identity_of(&mut h, 4242), Some(1942), "The Witcher 3");
    assert_eq!(identity_of(&mut h, 5151), Some(242408), "Counter-Strike 2");
    assert_eq!(
        identity_of(&mut h, 7777),
        Some(0),
        "a process the catalogue does not know sends 0"
    );
}

/// A quit game ends the hosted stream: the core reports the exited target
/// and the UI must route it to the normal stop path, not leave the session
/// streaming a dead process.
#[test]
fn quit_game_stops_the_hosted_stream() {
    let mut h = Harness::new();
    h.emit(Event::StreamTargetExited);

    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::StopStream)),
        "StreamTargetExited must emit Command::StopStream, got {cmds:?}"
    );
}

// ---------------------------------------------------------------------------
// The macOS Edit menu forwards its shortcuts to the text field
// ---------------------------------------------------------------------------
//
// On macOS the menu bar consumed Cmd+C, Cmd+V and the others, and the
// predefined menu items did nothing, so no text field received them. Our
// menu items now forward the key press with `edit_shortcuts::forward`.
// The testing backend has a clipboard, so Cut, Copy and Paste run for real.
// Not covered here: the real NSMenu, and the poll-loop arm that calls
// `forward`. Both need macOS and a human.

/// Open the step-3 email form and focus its Email field.
fn focus_step_three_email_field(h: &mut Harness) {
    resume(h.ctx(), OnboardingState::LinkIdentity);
    h.click_label("Email + password");
    h.click_label("Email");
    assert_eq!(h.field_text("Email"), "", "the field starts empty");
}

/// ★ Regression: Select All reached no text field on macOS. Typing after it
/// replaces the whole text.
#[test]
fn edit_menu_select_all_selects_the_text_in_the_focused_field() {
    let mut h = Harness::new();
    focus_step_three_email_field(&mut h);
    h.type_text("user@example.com");
    assert_eq!(h.field_text("Email"), "user@example.com");

    h.edit_menu(crate::edit_shortcuts::SELECT_ALL);
    h.type_text("x");

    assert_eq!(
        h.field_text("Email"),
        "x",
        "Select All must select the whole text, so typing replaces it"
    );
}

/// ★ Regression: Cut and Paste reached no text field on macOS.
#[test]
fn edit_menu_cut_and_paste_move_the_text_through_the_clipboard() {
    let mut h = Harness::new();
    focus_step_three_email_field(&mut h);
    h.type_text("user@example.com");

    h.edit_menu(crate::edit_shortcuts::SELECT_ALL);
    h.edit_menu(crate::edit_shortcuts::CUT);
    assert_eq!(h.field_text("Email"), "", "Cut must remove the selection");

    h.edit_menu(crate::edit_shortcuts::PASTE);
    assert_eq!(
        h.field_text("Email"),
        "user@example.com",
        "Paste must insert what Cut put on the clipboard"
    );
    h.edit_menu(crate::edit_shortcuts::PASTE);
    assert_eq!(h.field_text("Email"), "user@example.comuser@example.com");
}

/// ★ Regression: Copy reached no text field on macOS. Copy keeps the text.
#[test]
fn edit_menu_copy_puts_the_selection_on_the_clipboard() {
    let mut h = Harness::new();
    focus_step_three_email_field(&mut h);
    h.type_text("abc");

    h.edit_menu(crate::edit_shortcuts::SELECT_ALL);
    h.edit_menu(crate::edit_shortcuts::COPY);
    assert_eq!(h.field_text("Email"), "abc", "Copy must not remove text");

    // Typing replaces the selection. Paste then brings back the copied text.
    h.type_text("Z");
    assert_eq!(h.field_text("Email"), "Z");
    h.edit_menu(crate::edit_shortcuts::PASTE);
    assert_eq!(h.field_text("Email"), "Zabc");
}

/// ★ Regression: Undo and Redo reached no text field on macOS.
///
/// Slint 1.17 enables Redo on Control+Shift+Z on every target except Windows.
#[test]
fn edit_menu_undo_and_redo_step_through_the_edit_history() {
    let mut h = Harness::new();
    focus_step_three_email_field(&mut h);
    h.type_text("abc");
    assert_eq!(h.field_text("Email"), "abc");

    h.edit_menu(crate::edit_shortcuts::UNDO);
    assert_eq!(h.field_text("Email"), "", "Undo must remove the typed text");

    #[cfg(not(target_os = "windows"))]
    {
        h.edit_menu(crate::edit_shortcuts::REDO);
        assert_eq!(h.field_text("Email"), "abc", "Redo must restore the text");
    }
}

/// The macOS menu bar's Preferences item (Cmd+,) opens settings with the
/// user's name in the display-name field, as the settings button does. It had
/// its own copy of the open path, and that copy left the field empty.
#[cfg(target_os = "macos")]
#[test]
fn preferences_menu_item_shows_the_display_name_like_the_settings_button() {
    let mut h = Harness::new();
    h.app().set_user_name("b0bben".into());
    crate::poll_loop::handle_menu_item(h.ctx(), "prefs");
    h.pump();
    assert!(h.app().get_settings_open(), "Preferences opens settings");
    assert_eq!(h.field_text("DISPLAY NAME"), "b0bben");
}

/// Spec 10 section 8: every voice control reaches libmello. The
/// input-sensitivity slider and its auto toggle used to save only.
#[test]
fn input_sensitivity_controls_send_the_setting_to_core() {
    let mut h = Harness::new();
    let _ = h.commands();

    h.app().invoke_setting_changed_vad_threshold(-30.0);
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::SetInputSensitivity { auto: true, db } if *db == -30.0
        )),
        "the slider must send its level, got {cmds:?}"
    );

    h.app().invoke_setting_changed_vad_auto(false);
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::SetInputSensitivity { auto: false, db } if *db == -30.0
        )),
        "the auto toggle must send the mode with the saved level, got {cmds:?}"
    );
    let saved = h.settings();
    assert!(!saved.borrow().vad_auto);
    assert_eq!(saved.borrow().vad_threshold, -30.0);
}

// ---------------------------------------------------------------------------
// Invite share modal
// ---------------------------------------------------------------------------

/// Do what the event loop does between two frames, with no time passing:
/// create new conditional elements and run the `changed` handlers.
fn next_frame() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
}

/// Open the invite share modal the way production does: core reports a new
/// invite code. The modal is a conditional element, so it exists only after
/// the next frame.
fn open_invite_share_modal(h: &mut Harness) {
    logged_in_app(h);
    h.emit(Event::InviteCodeCreated {
        code: "ALPHA-992-K".into(),
    });
    assert!(h.app().get_invite_share_open());
    next_frame();
}

fn press_escape(h: &mut Harness) {
    let esc: slint::SharedString = slint::platform::Key::Escape.into();
    let window = h.app().window();
    window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: esc.clone() });
    window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: esc });
    h.pump();
}

/// Whether the copy button shows its copied state.
fn invite_shows_copied(h: &Harness) -> bool {
    ElementHandle::find_by_accessible_label(h.app(), "Copied")
        .next()
        .is_some()
}

/// Issue #86: Escape closes the invite modal.
#[test]
fn escape_closes_the_invite_share_modal() {
    let mut h = Harness::new();
    open_invite_share_modal(&mut h);

    press_escape(&mut h);

    assert!(!h.app().get_invite_share_open());
}

/// Issue #86: Escape also closes the invite modal after a click in the link
/// field moves the focus there.
#[test]
fn escape_closes_the_invite_share_modal_from_the_link_field() {
    let mut h = Harness::new();
    open_invite_share_modal(&mut h);
    h.click_label("Invite link");

    press_escape(&mut h);

    assert!(!h.app().get_invite_share_open());
}

/// The copied state reverts after 3 s, and a second copy in the same open
/// modal shows it again.
#[test]
fn a_second_invite_link_copy_shows_copied_again() {
    let mut h = Harness::new();
    open_invite_share_modal(&mut h);

    // Stand-in for `invite-copy-link`, which sets this after a clipboard
    // write. The test does not write the developer's real clipboard.
    h.app().set_invite_share_copied(true);
    next_frame();
    assert!(invite_shows_copied(&h), "the first copy shows Copied");

    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(3100));
    assert!(!invite_shows_copied(&h), "Copied reverts after 3 s");

    h.app().set_invite_share_copied(true);
    next_frame();
    assert!(invite_shows_copied(&h), "the second copy shows Copied");
}

// ---------------------------------------------------------------------------
// Member volume (crew panel popover, for this user only)
// ---------------------------------------------------------------------------

/// In an SFU call in Squad with bob, the crew panel on screen.
fn in_squad_with_bob_on_screen(h: &mut Harness) {
    use crate::CrewData;

    h.emit(Event::LoggedIn {
        user: sample_user(),
    });
    h.app()
        .set_crews(slint::ModelRc::new(slint::VecModel::from(vec![CrewData {
            id: "crew-1".into(),
            name: "Crew".into(),
            ..Default::default()
        }])));
    in_squad_with_bob(h, VoiceMode::SFU);
    next_frame();
}

/// The gains of the `SetPeerVolume` commands, as (peer, gain).
fn peer_volumes(h: &mut Harness) -> Vec<(String, f32)> {
    h.commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::SetPeerVolume { peer_id, volume } => Some((peer_id, volume)),
            _ => None,
        })
        .collect()
}

/// The volume and the local mute of `member` on its row in Squad.
fn squad_member_audio(h: &Harness, member: &str) -> (i32, bool) {
    let m = h
        .app()
        .get_voice_channels()
        .iter()
        .filter(|ch| ch.id == "ch-squad")
        .flat_map(|ch| ch.members.iter().collect::<Vec<_>>())
        .find(|m| m.id == member)
        .unwrap_or_else(|| panic!("no {member:?} in Squad"));
    (m.volume, m.local_muted)
}

/// Open the volume popover for `member` with its speaker glyph.
fn open_volume_popover(h: &mut Harness, member: &str) {
    h.click_label(&format!("Volume for {member}"));
    next_frame();
    assert_eq!(
        h.app().get_volume_member_id(),
        member,
        "the popover is open"
    );
}

fn volume_slider(h: &Harness, member: &str) -> ElementHandle {
    h.controls_labelled(&format!("Volume for {member}"))
        .into_iter()
        .find(|e| e.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Slider))
        .expect("the volume slider of the popover")
}

/// ★ The popover journey: 70 %, mute, unmute back at 70 %, reset to 100 %.
/// Each step sends the gain for bob's user id and shows on his row.
#[test]
fn the_member_volume_popover_sets_mutes_unmutes_and_resets() {
    let mut h = Harness::new();
    in_squad_with_bob_on_screen(&mut h);
    assert_eq!(squad_member_audio(&h, "u-bob"), (100, false));

    open_volume_popover(&mut h, "u-bob");
    assert!(
        !h.controls_labelled("Mute u-bob for you").is_empty(),
        "the mute switch is in the popover"
    );

    volume_slider(&h, "u-bob").set_accessible_value("70");
    h.pump();
    assert_eq!(peer_volumes(&mut h), vec![("u-bob".to_string(), 0.7)]);
    assert_eq!(squad_member_audio(&h, "u-bob"), (70, false));
    assert_eq!(
        h.app().get_volume_member().volume,
        70,
        "the popover shows it"
    );

    h.click_label("Mute u-bob for you");
    assert_eq!(peer_volumes(&mut h), vec![("u-bob".to_string(), 0.0)]);
    assert_eq!(
        squad_member_audio(&h, "u-bob"),
        (70, true),
        "the volume stays"
    );
    assert!(h.app().get_volume_member().local_muted);

    h.click_label("Mute u-bob for you");
    assert_eq!(
        peer_volumes(&mut h),
        vec![("u-bob".to_string(), 0.7)],
        "unmute returns to 70 %"
    );
    assert_eq!(squad_member_audio(&h, "u-bob"), (70, false));

    h.click_label("Reset volume to 100%");
    assert_eq!(peer_volumes(&mut h), vec![("u-bob".to_string(), 1.0)]);
    assert_eq!(squad_member_audio(&h, "u-bob"), (100, false));

    press_escape(&mut h);
    assert_eq!(h.app().get_volume_member_id(), "", "Escape closes it");
}

/// Your own row has no volume control, and a request for your own id
/// sends nothing.
#[test]
fn your_own_row_has_no_volume_control() {
    let mut h = Harness::new();
    in_squad_with_bob_on_screen(&mut h);

    assert!(
        !h.controls_labelled("Volume for u-bob").is_empty(),
        "another member has the control"
    );
    assert!(
        h.controls_labelled("Volume for u-me").is_empty(),
        "the local user has none"
    );

    h.app().invoke_member_volume_changed("u-me".into(), 50);
    h.app()
        .invoke_member_local_mute_changed("u-me".into(), true);
    assert!(peer_volumes(&mut h).is_empty());
    assert_eq!(squad_member_audio(&h, "u-me"), (100, false));
}

/// The popover closes when its member leaves the channel.
#[test]
fn the_volume_popover_closes_when_the_member_leaves() {
    use mello_core::crew_state::VoiceMember;

    let mut h = Harness::new();
    in_squad_with_bob_on_screen(&mut h);
    open_volume_popover(&mut h, "u-bob");

    h.emit(Event::VoiceUpdated {
        crew_id: "crew-1".into(),
        channel_id: "ch-squad".into(),
        members: vec![VoiceMember {
            user_id: "u-me".into(),
            username: "me".into(),
            ..Default::default()
        }],
    });
    assert_eq!(h.app().get_volume_member_id(), "");
}

/// ★ The stored value goes to libmello again when bob's peer appears
/// again: he leaves and comes back, and you leave and join again. The table
/// survives your leave.
#[test]
fn a_stored_member_volume_is_sent_again_when_the_member_comes_back() {
    let mut h = Harness::new();
    in_squad_with_bob_on_screen(&mut h);
    h.app().invoke_member_volume_changed("u-bob".into(), 70);
    assert_eq!(peer_volumes(&mut h), vec![("u-bob".to_string(), 0.7)]);

    h.emit(squad_update_with_bob_speaking(false));
    assert!(
        peer_volumes(&mut h).is_empty(),
        "bob stays: nothing to send"
    );

    // bob leaves and comes back.
    h.emit(Event::VoiceUpdated {
        crew_id: "crew-1".into(),
        channel_id: "ch-squad".into(),
        members: vec![mello_core::crew_state::VoiceMember {
            user_id: "u-me".into(),
            username: "me".into(),
            ..Default::default()
        }],
    });
    h.emit(squad_update_with_bob_speaking(false));
    assert_eq!(peer_volumes(&mut h), vec![("u-bob".to_string(), 0.7)]);
    assert_eq!(squad_member_audio(&h, "u-bob"), (70, false));

    // You leave the channel and join it again.
    h.emit(Event::VoiceStateChanged {
        in_call: false,
        transport: VoiceMode::Disconnected,
    });
    assert!(peer_volumes(&mut h).is_empty());
    h.emit(Event::VoiceJoined {
        crew_id: "crew-1".into(),
        channel_id: "ch-squad".into(),
        members: vec![
            mello_core::crew_state::VoiceMember {
                user_id: "u-me".into(),
                username: "me".into(),
                ..Default::default()
            },
            mello_core::crew_state::VoiceMember {
                user_id: "u-bob".into(),
                username: "bob".into(),
                ..Default::default()
            },
        ],
    });
    assert_eq!(
        peer_volumes(&mut h),
        vec![("u-bob".to_string(), 0.7)],
        "the rejoin sends the stored value"
    );
    assert_eq!(
        squad_member_audio(&h, "u-bob"),
        (70, false),
        "the leave kept it"
    );
}

/// Finishing onboarding sets start on boot from the share toggle, once.
/// The OS entry is behind the `autolaunch` test seam, never the real crate.
fn finalize_with_share(share: bool) -> (Harness, Option<bool>) {
    let h = Harness::new();
    h.app().set_onboarding_share_game_activity(share);
    h.settings().borrow_mut().start_on_boot = !share;
    finalize(&h);
    let requested = crate::autolaunch::last_requested();
    (h, requested)
}

#[test]
fn onboarding_with_share_off_leaves_start_on_boot_off() {
    let (h, requested) = finalize_with_share(false);
    assert!(!h.settings().borrow().start_on_boot);
    assert_eq!(requested, Some(false), "autolaunch is disabled");
}

#[test]
fn onboarding_with_share_on_turns_start_on_boot_on() {
    let (h, requested) = finalize_with_share(true);
    assert!(h.settings().borrow().start_on_boot);
    assert_eq!(requested, Some(true), "autolaunch is enabled");
}

#[test]
fn start_on_boot_defaults_on() {
    assert!(crate::Settings::default().start_on_boot);
}

// ---- installed games and the "Hidden from crew" list ----

fn installed_game(
    id: &str,
    name: &str,
    steam_hidden: bool,
) -> mello_core::events::InstalledGameInfo {
    mello_core::events::InstalledGameInfo {
        game_id: id.into(),
        name: name.into(),
        steam_hidden,
    }
}

fn games_tab_with_installed(h: &mut Harness) {
    // A tall window keeps every row inside the settings scroll area, where a
    // click can reach it.
    h.app()
        .window()
        .set_size(slint::PhysicalSize::new(1600, 3000));
    logged_in_app(h);
    h.emit(Event::InstalledGames {
        games: vec![
            installed_game("steam-620", "Portal 2", false),
            installed_game("steam-730", "Counter-Strike 2", false),
            installed_game("steam-999", "Steam Hidden Game", true),
        ],
    });
    h.app().set_settings_open(true);
    h.pump();
    h.click_label("GAMES");
}

fn hidden_command(cmds: &[Command]) -> Option<Vec<String>> {
    cmds.iter().rev().find_map(|c| match c {
        Command::SetHiddenGames { hidden } => Some(hidden.clone()),
        _ => None,
    })
}

#[test]
fn the_activity_sharing_section_uses_the_consent_copy() {
    let mut h = Harness::new();
    games_tab_with_installed(&mut h);
    assert!(
        !h.controls_labelled("Show crew what I play").is_empty(),
        "the share switch carries the new label"
    );
    assert!(
        h.controls_labelled("Show crew what I'm playing").is_empty(),
        "the old label is gone"
    );
    let texts = ElementHandle::find_by_accessible_label(
        h.app(),
        "Let crewmates see your installed games, what game you're in, and your play sessions in the crew feed.",
    )
    .count();
    assert!(texts > 0, "the section shows the new description");
}

#[test]
fn onboarding_toggle_and_subline_use_the_consent_copy() {
    let h = Harness::new();
    h.app().set_onboarding_step(2);
    assert!(
        !h.controls_labelled("Show crew what I play").is_empty(),
        "the onboarding switch carries the new label"
    );
    for line in [
        "Your installed games, play sessions and what you're in right now.",
        "m3llo starts with your PC and keeps track of your sessions.",
    ] {
        assert!(
            ElementHandle::find_by_accessible_label(h.app(), line).count() > 0,
            "missing line: {line}"
        );
    }
}

#[test]
fn hiding_a_game_sends_the_hidden_set_and_unhiding_clears_it() {
    let mut h = Harness::new();
    games_tab_with_installed(&mut h);
    let _ = h.commands();

    h.click_label("Hidden from crew: Portal 2");
    assert_eq!(
        hidden_command(&h.commands()),
        Some(vec!["steam-620".to_string()])
    );
    assert_eq!(h.settings().borrow().hidden_games, ["steam-620"]);

    h.click_label("Hidden from crew: Counter-Strike 2");
    assert_eq!(
        hidden_command(&h.commands()),
        Some(vec!["steam-620".to_string(), "steam-730".to_string()])
    );

    h.click_label("Hidden from crew: Portal 2");
    assert_eq!(
        hidden_command(&h.commands()),
        Some(vec!["steam-730".to_string()])
    );
    assert_eq!(h.settings().borrow().hidden_games, ["steam-730"]);
}

#[test]
fn a_game_hidden_in_steam_has_no_switch() {
    let mut h = Harness::new();
    games_tab_with_installed(&mut h);
    assert!(h
        .controls_labelled("Hidden from crew: Steam Hidden Game")
        .is_empty());
    assert!(!h.controls_labelled("Hidden from crew: Portal 2").is_empty());
}

/// The command from the toggle drives the report: the hidden game is absent
/// from the next report, and the unhidden one is back.
#[test]
fn toggling_hide_removes_the_game_from_the_next_report() {
    use mello_core::library::{LibraryEntry, LibrarySource};
    use mello_core::library_report::LibraryReporter;

    let entry = |id: &str, name: &str| LibraryEntry {
        source: LibrarySource::Steam,
        external_id: id.into(),
        name: name.into(),
        install_dir: std::path::PathBuf::from(format!("C:\\Games\\{name}")),
        playtime_min: Some(10),
        last_played_unix: None,
        hidden: false,
    };
    let mut reporter = LibraryReporter::new();
    reporter.set_scan(vec![
        entry("620", "Portal 2"),
        entry("730", "Counter-Strike 2"),
    ]);
    let none = std::collections::HashSet::new();
    let first = reporter.plan(true, true, &none).expect("first report");
    assert_eq!(first.len(), 2);

    let mut h = Harness::new();
    games_tab_with_installed(&mut h);
    let _ = h.commands();
    h.click_label("Hidden from crew: Portal 2");
    let hidden: std::collections::HashSet<String> = hidden_command(&h.commands())
        .expect("SetHiddenGames")
        .into_iter()
        .collect();

    let next = reporter.plan(true, true, &hidden).expect("the set changed");
    let ids: Vec<&str> = next.iter().map(|g| g.game_id.as_str()).collect();
    assert_eq!(ids, ["steam-730"], "the hidden game is not reported");
}

// ---- recap belts (CLIPS 10.2, HEAD2HEAD 6) ----

fn recap_entry(extra: serde_json::Value) -> FeedEntry {
    let mut data = serde_json::json!({
        "crew_id": "crew-0",
        "week_start": 1, "week_end": 2,
        "total_hangout_min": 620, "top_game": "Rocket League",
        "longest_session": "Max in General (245m)", "longest_session_min": 245,
        "clip_count": 9, "most_active": "Max",
        "top_members": [{"user_id": "user-1", "display_name": "Max", "hangout_min": 300}],
    });
    for (k, v) in extra.as_object().expect("object").iter() {
        data[k] = v.clone();
    }
    feed_entry("recap-1", "recap", data)
}

fn belt_moved(title: &str, value: serde_json::Value, unit: &str) -> serde_json::Value {
    serde_json::json!({
        "title_id": title, "value": value, "unit": unit,
        "holder_user_id": "user-1", "holder_name": "Jonas",
        "previous_holder_user_id": "user-2", "previous_holder_name": "Bob",
        "moved_at_ms": 1,
    })
}

fn belts_held() -> serde_json::Value {
    serde_json::json!([
        {"user_id": "user-1", "display_name": "Jonas", "count": 3, "title_ids": ["the_carry", "clipper", "mouth"]},
        {"user_id": "user-2", "display_name": "Bob", "count": 1, "title_ids": ["night_owl"]},
    ])
}

/// The user opened the recap of the week that starts at `week_start`, so the
/// feed draws the card and not the sealed one.
fn open_recap(h: &Harness, week_start: i64) {
    h.settings()
        .borrow_mut()
        .sealed_opened
        .insert("crew-0".into(), vec![crate::sealed::recap_key(week_start)]);
}

fn emit_recap(h: &mut Harness, extra: serde_json::Value) {
    app_with_crew_of(h, 2);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![recap_entry(extra)]),
    });
}

/// A recap with two moved belts draws two rows, each a title chip, and the
/// held section lists the members in count order.
#[test]
fn recap_with_two_moved_belts_shows_two_rows_and_the_held_section() {
    let mut h = Harness::new();
    emit_recap(
        &mut h,
        serde_json::json!({
            "belts_moved": [
                belt_moved("the_carry", serde_json::json!(4201.0), "h"),
                belt_moved("night_owl", serde_json::json!("03:40"), ""),
            ],
            "belts_held": belts_held(),
            "leaderboard": belts_held(),
            // The belt awards must not show twice, as rows of the games list.
            "awards": [
                {"kind": "belt", "title": "The Carry", "name": "Jonas", "detail": "took it from Bob"},
            ],
        }),
    );
    let card = first_card(&h);
    assert_eq!(card.card_type.as_str(), "recap");
    assert_eq!(card.belts_moved.row_count(), 2);
    assert_eq!(count_of(&h, "TitleChip"), 2, "one chip for each moved belt");
    let first = card.belts_moved.row_data(0).expect("row");
    assert_eq!(first.title.as_str(), "The Carry");
    assert_eq!(first.value.as_str(), "4,201");
    assert_eq!(first.unit.as_str(), "h");
    assert_eq!(first.prev_name.as_str(), "Bob");
    assert_eq!(first.holder_name.as_str(), "Jonas");
    assert_eq!(card.belts_held.row_count(), 2);
    let top = card.belts_held.row_data(0).expect("row");
    assert_eq!((top.name.as_str(), top.count), ("Jonas", 3));
    assert_eq!(top.titles.row_count(), 3);
    assert_eq!(
        card.awards.row_count(),
        0,
        "a belt award is not a game award"
    );
    // The hangout numbers stay.
    assert_eq!(card.game_name.as_str(), "Rocket League");
}

/// The tall card shows two moved belts and "+N more". The full-screen recap
/// shows all of them.
#[test]
fn recap_card_caps_moved_belts_at_two_and_opens_full_screen_with_all() {
    let mut h = Harness::new();
    emit_recap(
        &mut h,
        serde_json::json!({
            "belts_moved": [
                belt_moved("the_carry", serde_json::json!(10.0), "h"),
                belt_moved("night_owl", serde_json::json!("03:40"), ""),
                belt_moved("sampler", serde_json::json!(9), "games"),
                belt_moved("clipper", serde_json::json!(4), "clips"),
            ],
            "belts_held": belts_held(),
        }),
    );
    assert_eq!(count_of(&h, "RecapOpened"), 0);
    assert_eq!(
        count_of(&h, "TitleChip"),
        2,
        "the card shows two of the four rows: a clipped row is absent from queries"
    );
    assert!(
        drawn_texts(&h).iter().any(|t| t == "+2 MORE"),
        "the rest is a count"
    );
    h.click_label("See the week");
    let w = h.app().window().size().width;
    h.app().window().set_size(slint::PhysicalSize::new(w, 3600));
    assert_eq!(
        count_of(&h, "RecapOpened"),
        1,
        "the card opens the full-screen recap"
    );
    assert_eq!(count_of(&h, "TitleChip"), 2, "the card keeps its two rows");
    assert_eq!(
        count_of(&h, "BeltName"),
        4,
        "all four rows in the full-screen recap"
    );
    h.click_label("Close weekly recap");
    assert_eq!(count_of(&h, "RecapOpened"), 0);
}

/// A recap with no belts shows neither section.
#[test]
fn recap_without_belts_shows_neither_section() {
    let mut h = Harness::new();
    emit_recap(&mut h, serde_json::json!({}));
    let card = first_card(&h);
    assert_eq!(card.card_type.as_str(), "recap");
    assert_eq!(card.belts_moved.row_count(), 0);
    assert_eq!(card.belts_held.row_count(), 0);
    assert_eq!(count_of(&h, "TitleChip"), 0);
    h.click_label("See the week");
    assert_eq!(
        count_of(&h, "TitleChip"),
        0,
        "no belts, no chips in the full-screen recap"
    );
}

// ---- the tall recap card (mockup weekly-recap-card, frame 1) ----

/// Sessions for the cells under head2head in frame F.
fn tall_recap_crew(recap: serde_json::Value) -> Harness {
    let mut h = Harness::new();
    // The recap of week 1 is open: the card shows, not the sealed one.
    open_recap(&h, 1);
    app_with_crew_of(&mut h, 5);
    set_roster(
        &h,
        &[
            ("user-1", "Test User"),
            ("u2", "Jonas"),
            ("u3", "Sara"),
            ("u4", "Max"),
            ("u5", "Leo"),
        ],
    );
    let session = |id: &str| {
        feed_entry(
            id,
            "session",
            serde_json::json!({"game_name": "Valorant", "duration_min": 60, "active_min": 60}),
        )
    };
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![
            h2h_entry(
                5,
                vec![h2h_title("the_carry", "u2", serde_json::json!(4201.0), "h")],
                vec![
                    h2h_pair("u2", 1, 3),
                    h2h_pair("u3", 2, 2),
                    h2h_pair("u4", 3, 1),
                    h2h_pair("u5", 2, 2),
                ],
                None,
            ),
            recap_entry(recap),
            session("s1"),
            session("s2"),
        ]),
    });
    h
}

fn week_with_everything() -> serde_json::Value {
    serde_json::json!({
        "week_number": 4,
        "total_min": 1180, "total_hangout_min": 1180, "longest_session_min": 228,
        "clip_count": 11,
        "top_members": [{"user_id": "u4", "display_name": "Max", "hangout_min": 550}],
        "most_clipped": "Leo", "most_clipped_user_id": "u5", "most_clipped_count": 5,
        "belts_moved": [
            belt_moved("the_carry", serde_json::json!(4201.0), "h"),
            belt_moved("night_owl", serde_json::json!("03:40"), ""),
            belt_moved("sampler", serde_json::json!(9), "games"),
        ],
        "belts_held": belts_held(),
        "week_start": 1, "week_end": 2,
        "clip_of_week": {
            "clip_id": "c1", "title": "Leo clipped that", "duration_seconds": 29.0,
            "game": "Rocket League", "clipper_name": "Leo",
            "participant_names": ["Bob", "Max", "Sara", "Leo"],
            "ts": 3_600_000, "media_url": "https://example.invalid/c1.opus",
        },
        "games_played": [
            {"game": "Rocket League", "matches": 4, "minutes": 740},
            {"game": "Counter-Strike 2", "matches": 3, "minutes": 550},
            {"game": "Fall Guys", "matches": 0, "minutes": 270},
            {"game": "Dota 2", "matches": 0, "minutes": 220},
            {"game": "Night Stones", "matches": 0, "minutes": 120},
        ],
    })
}

fn week_without_belts() -> serde_json::Value {
    let mut w = week_with_everything();
    w["belts_moved"] = serde_json::json!([]);
    w["belts_held"] = serde_json::json!([]);
    w
}

const TALL_SECTIONS: [&str; 5] = [
    "RecapHead",
    "RecapPeople",
    "RecapMoved",
    "RecapHeld",
    "RecapGamesStrip",
];

fn rect_of(e: &ElementHandle) -> (f32, f32, f32, f32) {
    let (p, s) = (e.absolute_position(), e.size());
    (p.x, p.y, p.x + s.width, p.y + s.height)
}

fn overlap(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.2 - 0.5 && b.0 < a.2 - 0.5 && a.1 < b.3 - 0.5 && b.1 < a.3 - 0.5
}

/// With belts the card has every section and three grid rows.
#[test]
fn the_tall_recap_has_every_section_and_three_rows_with_belts() {
    let h = tall_recap_crew(week_with_everything());
    for name in TALL_SECTIONS {
        assert_eq!(count_of(&h, name), 1, "{name} is on the card");
    }
    assert_eq!(count_of(&h, "RecapFoot"), 1);
    let card = ElementHandle::find_by_element_type_name(h.app(), "RecapSlot")
        .next()
        .expect("the recap");
    let rows = (card.size().height + 12.0) / (232.0 + 12.0);
    assert!((rows - 3.0).abs() < 0.01, "three rows, got {rows}");
    let texts = drawn_texts(&h);
    for want in [
        "WEEKLY RECAP",
        "HANGOUT",
        "CLIPS",
        "LONGEST",
        "TOP GAME",
        "MOST ACTIVE",
        "MOST CLIPPED",
        "BELTS MOVED",
        "+1 MORE",
        "BELTS HELD",
        "GAMES PLAYED",
        "Max",
        "Leo",
        "9 h 10",
    ] {
        assert!(texts.iter().any(|t| t == want), "{want:?} in {texts:?}");
    }
    assert_eq!(
        count_of(&h, "TitleChip"),
        2,
        "two belts and the rest a count"
    );
    // Four tiles with the hours of each game, the fifth game is for the opened
    // recap.
    assert!(texts.iter().any(|t| t == "12 H 20"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "9 H 10"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "3 H 40"), "{texts:?}");
    assert!(!texts.iter().any(|t| t == "2 H 00"), "{texts:?}");
    // No note of the mockup stays on the card: no day, no source.
    for note in ["Monday", "Head2head", "Now", "NOW", "31 h 40 together"] {
        assert!(!texts.iter().any(|t| t == note), "{note:?} in {texts:?}");
    }
}

/// With no belts both belt sections are gone and the card is two rows.
#[test]
fn the_tall_recap_without_belts_has_no_belt_sections_and_two_rows() {
    let h = tall_recap_crew(week_without_belts());
    assert_eq!(count_of(&h, "RecapMoved"), 0);
    assert_eq!(count_of(&h, "RecapHeld"), 0);
    for name in ["RecapHead", "RecapPeople", "RecapGamesStrip", "RecapFoot"] {
        assert_eq!(count_of(&h, name), 1, "{name} is on the card");
    }
    let card = ElementHandle::find_by_element_type_name(h.app(), "RecapSlot")
        .next()
        .expect("the recap");
    let rows = (card.size().height + 12.0) / (232.0 + 12.0);
    assert!((rows - 2.0).abs() < 0.01, "two rows, got {rows}");
    assert_eq!(first_recap(&h).recap_rows, 2);
}

fn first_recap(h: &Harness) -> FeedCardData {
    use slint::Model;
    h.app()
        .get_feed_cards()
        .iter()
        .find(|c| c.card_type == "recap")
        .expect("a recap card")
}

/// "See the week" stands in a row of its own: it touches no section.
#[test]
fn see_the_week_intersects_no_sibling() {
    for week in [week_with_everything(), week_without_belts()] {
        let h = tall_recap_crew(week);
        let controls = h.controls_labelled("See the week");
        assert_eq!(controls.len(), 1);
        let button = rect_of(&controls[0]);
        let card = ElementHandle::find_by_element_type_name(h.app(), "RecapSlot")
            .next()
            .expect("the recap");
        let card = rect_of(&card);
        assert!(
            button.2 <= card.2 && button.3 <= card.3 && button.0 >= card.0,
            "the button {button:?} is inside the card {card:?}"
        );
        for name in TALL_SECTIONS {
            for e in ElementHandle::find_by_element_type_name(h.app(), name) {
                assert!(
                    !overlap(button, rect_of(&e)),
                    "the button {button:?} sits on {name} {:?}",
                    rect_of(&e)
                );
            }
        }
    }
}

/// The server's new fields reach the card: the most clipped member with the
/// count, the hours of every game and the week.
#[test]
fn the_recap_card_reads_the_most_clipped_member_the_games_and_the_week() {
    let h = tall_recap_crew(week_with_everything());
    let card = first_recap(&h);
    assert_eq!(card.recap_rows, 3);
    assert_eq!(card.recap_week.as_str(), "Week 4");
    assert_eq!(card.recap_active.name.as_str(), "Max");
    assert_eq!(card.recap_active.count, 550);
    assert_eq!(card.recap_clipped.name.as_str(), "Leo");
    assert_eq!(card.recap_clipped.count, 5);
    assert_eq!(card.recap_games.row_count(), 5, "every game is on the data");
    assert_eq!(card.recap_games_min, 1900);
    let first = card.recap_games.row_data(0).expect("a game");
    assert_eq!((first.name.as_str(), first.minutes), ("Rocket League", 740));
}

/// A recap stored before `most_clipped_count` shows no most clipped line and
/// no games strip, and keeps the card.
#[test]
fn an_older_recap_draws_without_the_new_fields() {
    let h = tall_recap_crew(serde_json::json!({"most_clipped": "Leo"}));
    let card = first_recap(&h);
    assert_eq!(card.recap_clipped.name.as_str(), "");
    assert_eq!(card.recap_games.row_count(), 0);
    assert_eq!(count_of(&h, "RecapGamesStrip"), 0);
    assert_eq!(count_of(&h, "RecapHead"), 1);
}

// ---- the opened recap (mockup weekly-recap-card, frame 2) ----

fn open_the_week(h: &mut Harness) {
    h.click_label("See the week");
    // A window tall enough for every section: what is below the scroll area
    // is absent from queries.
    let w = h.app().window().size().width;
    h.app().window().set_size(slint::PhysicalSize::new(w, 3600));
    assert_eq!(count_of(h, "RecapOpened"), 1);
}

/// Every section of frame 2 is there, with every game and its hours.
#[test]
fn the_opened_recap_shows_every_section() {
    let mut h = tall_recap_crew(week_with_everything());
    open_the_week(&mut h);
    let texts = drawn_texts(&h);
    for want in [
        "THIS WEEK",
        "HANGOUT TIME",
        "TOP GAME",
        "LONGEST SESSION",
        "CLIPS CAPTURED",
        "MOST ACTIVE",
        "MOST CLIPPED",
        "BELTS MOVED THIS WEEK",
        "WAS HELD BY",
        "NOW HELD BY",
        "BELTS HELD",
        "RANK 1",
        "CLIP OF THE WEEK",
        "Leo clipped that",
        "0:29",
        "GAMES PLAYED",
        "31 h 40",
        "Rocket League",
        "Counter-Strike 2",
        "Fall Guys",
        "Dota 2",
        "Night Stones",
        "12 H 20",
        "9 H 10",
        "4 H 30",
        "3 H 40",
        "2 H 00",
    ] {
        assert!(texts.iter().any(|t| t == want), "{want:?} in {texts:?}");
    }
    assert_eq!(count_of(&h, "BeltName"), 3, "every moved belt");
    // The held section lists each member once with a rank.
    assert!(texts.iter().any(|t| t == "RANK 2"), "{texts:?}");
    // No source note stays.
    for note in ["HEAD2HEAD · NOW", "31 h 40 together"] {
        assert!(!texts.iter().any(|t| t == note), "{note:?} in {texts:?}");
    }
}

/// A week with no belts, no clip and no games has only the numbers.
#[test]
fn the_opened_recap_leaves_out_what_the_week_did_not_have() {
    let mut h = tall_recap_crew(serde_json::json!({
        "total_min": 90, "clip_count": 0,
        "top_members": [{"user_id": "u4", "display_name": "Max", "hangout_min": 90}],
    }));
    open_the_week(&mut h);
    let texts = drawn_texts(&h);
    assert!(texts.iter().any(|t| t == "THIS WEEK"), "{texts:?}");
    for gone in [
        "BELTS MOVED THIS WEEK",
        "BELTS HELD",
        "CLIP OF THE WEEK",
        "GAMES PLAYED",
    ] {
        assert!(!texts.iter().any(|t| t == gone), "{gone:?} in {texts:?}");
    }
}

/// The play ring plays the clip of the week through the clip player.
#[test]
fn the_play_ring_of_the_clip_of_the_week_plays_the_clip() {
    let mut h = tall_recap_crew(week_with_everything());
    open_the_week(&mut h);
    h.click_label("Play");
    assert!(
        h.commands().iter().any(|c| matches!(
            c,
            Command::PlayClip { path } if path == "https://example.invalid/c1.opus"
        )),
        "the ring sends the clip to the player: {:?}",
        h.commands()
    );
}

/// The recap is min(880, panel less 48) wide, centred in the feed panel.
#[test]
fn the_opened_recap_is_880_wide_or_the_panel_less_48_and_centred() {
    let mut h = tall_recap_crew(week_with_everything());
    open_the_week(&mut h);
    for panel_w in [1100.0_f32, 700.0] {
        let win = h.app().window().size();
        let (_, _, w0, _) = {
            let c = ElementHandle::find_by_element_type_name(h.app(), "CrewFeed")
                .next()
                .expect("feed");
            let r = rect_of(&c);
            (r.0, r.1, r.2 - r.0, r.3 - r.1)
        };
        h.app().window().set_size(slint::PhysicalSize::new(
            (win.width as f32 + panel_w - w0) as u32,
            win.height,
        ));
        let feed = ElementHandle::find_by_element_type_name(h.app(), "CrewFeed")
            .next()
            .map(|e| rect_of(&e))
            .expect("feed");
        let section = ElementHandle::find_by_element_type_name(h.app(), "Section")
            .next()
            .map(|e| rect_of(&e))
            .expect("a section");
        let feed_w = feed.2 - feed.0;
        let want = 880.0_f32.min(feed_w - 48.0);
        assert!(
            ((section.2 - section.0) - want).abs() < 1.5,
            "panel {feed_w}: section {} wide, want {want}",
            section.2 - section.0
        );
        let left = section.0 - feed.0;
        let right = feed.2 - section.2;
        assert!((left - right).abs() < 1.5, "centred: {left} and {right}");
    }
}

/// A crew of one has no weekly recap: with nobody to be in voice with and no
/// clip, the week is empty (`isEmptyRecap`), so no card is drawn and the recap
/// skeleton stays. The rule did not change with the tall card.
#[test]
fn a_crew_of_one_renders_no_recap() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![recap_entry(serde_json::json!({
            "total_hangout_min": 0, "total_game_min": 0, "total_min": 0,
            "clip_count": 0, "top_members": [], "most_clipped": "",
        }))]),
    });
    assert_eq!(count_of(&h, "RecapSlot"), 0);
    assert_eq!(count_of(&h, "RecapCard"), 0);
    assert_eq!(count_of(&h, "RecapHead"), 0);
    assert!(
        h.app().get_feed_skeleton_recap(),
        "the recap skeleton stays"
    );
}

/// A title this client has no icon for still gets a name and the belt icon.
#[test]
fn recap_names_session_titles_the_card_has_no_chip_for() {
    let mut h = Harness::new();
    emit_recap(
        &mut h,
        serde_json::json!({
            "belts_moved": [belt_moved("last_one_up", serde_json::json!(5), "nights")],
        }),
    );
    let row = first_card(&h).belts_moved.row_data(0).expect("row");
    assert_eq!(row.title.as_str(), "Last One Up");
}

fn recap_feed(data: serde_json::Value) -> mello_core::crew_events::FeedResponse {
    mello_core::crew_events::FeedResponse {
        crew_id: "crew-0".into(),
        sections: vec![mello_core::crew_events::FeedSection {
            id: "this_week".into(),
            entries: vec![mello_core::crew_events::FeedEntry {
                id: "recap_1".into(),
                entry_type: "recap".into(),
                role: "recap".into(),
                size: "md".into(),
                ts: 1,
                data,
            }],
        }],
        joined: vec![],
    }
}

fn recap_titles(h: &Harness) -> Vec<String> {
    use slint::Model;
    h.app()
        .get_feed_cards()
        .iter()
        .filter(|c| c.card_type == "recap")
        .map(|c| c.title.to_string())
        .collect()
}

/// A recap with game minutes only leads with the game time (total_min), and an
/// older recap without total_min falls back to total_hangout_min.
#[test]
fn recap_card_leads_with_total_min() {
    let mut h = Harness::new();
    logged_in_app(&mut h);
    h.emit(Event::FeedLoaded {
        response: recap_feed(serde_json::json!({
            "total_hangout_min": 0, "total_game_min": 90, "total_min": 90,
            "session_count": 2, "clip_count": 0,
        })),
    });
    assert_eq!(recap_titles(&h), vec!["1.5h".to_string()]);

    h.emit(Event::FeedLoaded {
        response: recap_feed(serde_json::json!({
            "total_hangout_min": 45, "clip_count": 0,
        })),
    });
    assert_eq!(recap_titles(&h), vec!["45m".to_string()]);
}

/// A clips-only week leads with the clip count, never "0m", and keeps the card.
#[test]
fn clips_only_recap_leads_with_clip_count() {
    let mut h = Harness::new();
    logged_in_app(&mut h);
    h.emit(Event::FeedLoaded {
        response: recap_feed(serde_json::json!({
            "total_hangout_min": 0, "total_game_min": 0, "total_min": 0,
            "session_count": 0, "clip_count": 3,
        })),
    });
    assert_eq!(recap_titles(&h), vec!["3 clips".to_string()]);
}

/// An empty recap that reaches the client renders no recap card.
#[test]
fn empty_recap_renders_no_recap_card() {
    let mut h = Harness::new();
    logged_in_app(&mut h);
    h.emit(Event::FeedLoaded {
        response: recap_feed(serde_json::json!({
            "total_hangout_min": 0, "total_game_min": 0, "total_min": 0,
            "session_count": 0, "clip_count": 0,
        })),
    });
    assert!(recap_titles(&h).is_empty());
}

fn clip_entry(id: &str) -> FeedEntry {
    feed_entry(
        id,
        "clip",
        serde_json::json!({"duration_seconds": 12.0, "media_url": "/clips/x.wav", "clipper_name": "alice"}),
    )
}

fn feed_card_types(h: &Harness) -> Vec<String> {
    let m = h.app().get_feed_cards();
    (0..m.row_count())
        .filter_map(|i| m.row_data(i))
        .map(|c| c.card_type.to_string())
        .collect()
}

fn big_crew_feed(clips: usize) -> FeedResponse {
    let mut entries = vec![h2h_entry(
        50,
        vec![h2h_title(
            "the_collector",
            "u2",
            serde_json::json!(14),
            "installed",
        )],
        vec![h2h_pair("u2", 1, 1), h2h_pair("u3", 1, 1)],
        None,
    )];
    for i in 0..clips {
        entries.push(clip_entry(&format!("clip{i}")));
    }
    entries.push(feed_entry(
        "recent_games",
        "recent-games",
        serde_json::json!({
            "session_count": 14, "total_min": 1860,
            "games": [{ "game_id": "valorant", "game_name": "Valorant", "minutes": 600 }]
        }),
    ));
    entries.push(feed_entry(
        "s1",
        "session",
        serde_json::json!({"game_name": "Valorant", "duration_min": 60, "active_min": 60}),
    ));
    feed_of(entries)
}

/// A voice clip never takes a wide slot (CLIPS 7.1). With a head2head cell,
/// the invite card and five clips, the wide slots hold no clip.
#[test]
fn five_clips_never_take_a_wide_slot() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 50);
    set_roster(
        &h,
        &[("user-1", "Test User"), ("u2", "Jonas"), ("u3", "Sara")],
    );
    h.emit(Event::FeedLoaded {
        response: big_crew_feed(5),
    });

    let types = feed_card_types(&h);
    let infos: Vec<crate::feed_grid::CardInfo> = {
        let m = h.app().get_feed_cards();
        (0..m.row_count())
            .filter_map(|i| m.row_data(i))
            .map(|c| crate::feed_grid::CardInfo::of(&c))
            .collect()
    };
    let wide = crate::feed_grid::wide_slot_indices(&infos, 0);
    assert!(!wide.is_empty(), "the layout has a wide slot: {types:?}");
    for w in wide {
        assert_ne!(types[w], "clip", "a clip is in wide slot {w}: {types:?}");
    }
}

/// Recent games is on the first screen for a big crew: in the first two rows
/// of the plain grid (head2head, invite and one card, then one card and the
/// wide slot), not behind the clips.
#[test]
fn recent_games_is_in_the_first_two_rows_of_a_big_crew() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 50);
    set_roster(
        &h,
        &[("user-1", "Test User"), ("u2", "Jonas"), ("u3", "Sara")],
    );
    h.emit(Event::FeedLoaded {
        response: big_crew_feed(5),
    });

    let types = feed_card_types(&h);
    let at = types
        .iter()
        .position(|t| t == "recent-games")
        .expect("recent games card");
    assert!(
        at <= 4,
        "recent games at card {at}, past the second row: {types:?}"
    );
}

/// A session card whose game has no art wears the two-letter monogram tile of
/// the recent games card. The long name never reaches a square tile: "Brawl",
/// "Terrar" and "Apex" were cut off there.
#[test]
fn a_session_card_without_art_wears_the_two_letter_monogram() {
    for (name, mono) in [
        ("Brawlhalla", "BR"),
        ("Terraria", "TE"),
        ("Apex Legends", "AL"),
    ] {
        let mut h = Harness::new();
        logged_in_app(&mut h);
        let mut feed = zero_telemetry_game_session_feed();
        feed.sections[0].entries[0].data["game_name"] = serde_json::json!(name);
        feed.sections[0].entries[0].data["game_id"] = serde_json::json!("custom-no-art");
        h.emit(Event::FeedLoaded { response: feed });

        let card = first_card(&h);
        assert_eq!(card.game_monogram.as_str(), mono, "{name}");
        assert!(
            !card.game_has_icon && !card.game_has_capsule,
            "{name} has no art"
        );
        assert_eq!(
            card.game_monogram.as_str(),
            crate::recent_games::monogram(name),
            "{name}: the same monogram as the recent games card"
        );
        assert_eq!(
            count_of(&h, "GameArt"),
            1,
            "{name}: the session card draws the shared game tile"
        );
    }
}

/// A crew mate's session or clip makes the server push "feed changed". The UI
/// hands it to the core, which coalesces to one reload per 5 s
/// (`mello_core::feed_reload`). Here: every push becomes one command, and the
/// core gate decides.
#[test]
fn a_feed_changed_push_goes_to_the_core_as_a_command() {
    let mut h = Harness::new();
    logged_in_app(&mut h);
    for _ in 0..3 {
        h.emit(Event::FeedChanged {
            crew_id: "crew-0".into(),
        });
    }
    let n = h
        .commands()
        .iter()
        .filter(|c| matches!(c, Command::FeedChanged { crew_id } if crew_id == "crew-0"))
        .count();
    assert_eq!(
        n, 3,
        "the gate lives in the core, one command for each push"
    );
}

/// A crew mate joined and the feed reloaded on a push. The crew list still
/// says one member, the head2head entry of the feed says more. At two members
/// the trio stays (frame B2). From three it goes (frame C).
#[test]
fn the_feed_member_count_drives_the_skeleton_trio_before_the_crew_list_catches_up() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);
    set_roster(
        &h,
        &[("user-1", "Test User"), ("u2", "Jonas"), ("u3", "Max")],
    );
    let feed_of_members = |members: i32| {
        let mut feed = recent_games_feed();
        feed.sections[0].entries.insert(
            0,
            h2h_entry(
                members,
                vec![h2h_title(
                    "the_collector",
                    "u2",
                    serde_json::json!(14),
                    "installed",
                )],
                vec![h2h_pair("u2", 1, 1)],
                None,
            ),
        );
        feed
    };
    h.emit(Event::FeedLoaded {
        response: feed_of_members(2),
    });
    assert_eq!(skeleton_cells(&h), 3, "two members: the trio stays");
    h.emit(Event::FeedLoaded {
        response: feed_of_members(3),
    });
    assert_eq!(skeleton_cells(&h), 0, "three members: no trio");
}

// ── live session card ───────────────────────────────────────────────

fn game_detected(game_id: &str, name: &str, started_at_ms: i64) -> Event {
    Event::GameDetected {
        game_id: game_id.into(),
        game_name: name.into(),
        short_name: "G".into(),
        color: "#2A2A30".into(),
        pid: 42,
        exe_path: String::new(),
        started_at_ms,
    }
}

fn game_ended(game_id: &str, name: &str, minutes: u32) -> Event {
    Event::GameEnded {
        game_id: game_id.into(),
        game_name: name.into(),
        short_name: "G".into(),
        duration_min: minutes,
    }
}

/// A crewmate's presence push, as the server sends it.
fn mate_plays(user: &str, game_id: &str, name: &str, ago_min: i64) -> Event {
    Event::PresenceChanged {
        change: mello_core::crew_state::PresenceChange {
            crew_id: "crew-0".into(),
            user_id: user.into(),
            presence: mello_core::crew_state::PresenceInfo {
                status: mello_core::presence::PresenceStatus::Online,
                activity: None,
                game: Some(mello_core::presence::GamePresence {
                    game_name: name.into(),
                    game_id: game_id.into(),
                    started_at: mello_core::presence::to_rfc3339(now_ms() - ago_min * 60_000),
                }),
            },
        },
    }
}

/// Local Steam poster art for a game installed on this device: a Steam cache
/// in a temp folder with `library_hero.jpg`, the installed list that knows the
/// game by name, and the art lookup of this thread pointed at the cache. The
/// returned guard clears the lookup and the folder.
struct PosterArt(std::path::PathBuf);

impl Drop for PosterArt {
    fn drop(&mut self) {
        mello_core::library::set_steam_root_override(None);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn install_with_poster(h: &Harness, appid: &str, name: &str) -> PosterArt {
    let root = std::env::temp_dir().join(format!(
        "m3llo-poster-{}-{appid}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("appcache").join("librarycache").join(appid);
    std::fs::create_dir_all(&dir).expect("poster dir");
    let mut jpg = Vec::new();
    let art = image::RgbImage::from_pixel(96, 31, image::Rgb([200, 120, 40]));
    image::codecs::jpeg::JpegEncoder::new(&mut jpg)
        .encode_image(&art)
        .expect("encode the poster");
    std::fs::write(dir.join("library_hero.jpg"), jpg).expect("poster file");
    mello_core::library::set_steam_root_override(Some(root.clone()));
    h.ctx()
        .installed_games
        .borrow_mut()
        .push(mello_core::events::InstalledGameInfo {
            game_id: format!("steam-{appid}"),
            name: name.into(),
            steam_hidden: false,
        });
    PosterArt(root)
}

/// A Steam cache with `header.jpg` for one app, in a temp folder. The art
/// lookup of this thread points at it. With `installed` the installed list
/// knows the game by name, as a scan would leave it.
fn install_with_header(h: &Harness, appid: &str, name: &str, installed: bool) -> PosterArt {
    let root = std::env::temp_dir().join(format!(
        "m3llo-header-{}-{appid}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("appcache").join("librarycache").join(appid);
    std::fs::create_dir_all(&dir).expect("header dir");
    let mut jpg = Vec::new();
    let art = image::RgbImage::from_pixel(92, 43, image::Rgb([40, 120, 200]));
    image::codecs::jpeg::JpegEncoder::new(&mut jpg)
        .encode_image(&art)
        .expect("encode the header");
    std::fs::write(dir.join("header.jpg"), jpg).expect("header file");
    mello_core::library::set_steam_root_override(Some(root.clone()));
    if installed {
        h.ctx()
            .installed_games
            .borrow_mut()
            .push(mello_core::events::InstalledGameInfo {
                game_id: format!("steam-{appid}"),
                name: name.into(),
                steam_hidden: false,
            });
    }
    PosterArt(root)
}

/// One game id, one kind of art on every surface. Brawlhalla has Steam art on
/// disk. The recent games tile (library id or catalogue id), the session card
/// and the NOW PLAYING pill all show it. Before the shared lookup, the tile
/// and the pill showed the monogram when the id was the catalogue id.
#[test]
fn one_game_id_resolves_to_the_same_art_on_tile_session_card_and_pill() {
    // (installed list knows the game, id the recent games row carries)
    for (installed, wire_id) in [
        (true, "steam-291550"),
        (true, "brawlhalla"),
        (false, "brawlhalla"),
        (false, "steam-291550"),
    ] {
        let mut h = Harness::new();
        h.app()
            .window()
            .set_size(slint::PhysicalSize::new(1600, 1000));
        logged_in_app(&mut h);
        let _art = install_with_header(&h, "291550", "Brawlhalla", installed);
        let case = format!("installed {installed}, id {wire_id}");

        // Recent games tile.
        h.emit(Event::FeedLoaded {
            response: feed_of(vec![recent_games_entry(serde_json::json!([
                { "game_id": wire_id, "game_name": "Brawlhalla", "minutes": 2500, "launcher": "steam" }
            ]))]),
        });
        let tile = first_card(&h).recent_games.row_data(0).expect("a tile");
        assert!(tile.has_capsule, "{case}: the tile has Steam art");

        // Session card.
        let mut feed = zero_telemetry_game_session_feed();
        feed.sections[0].entries[0].data["game_name"] = serde_json::json!("Brawlhalla");
        feed.sections[0].entries[0].data["game_id"] = serde_json::json!("brawlhalla");
        h.emit(Event::FeedLoaded { response: feed });
        let card = first_card(&h);
        assert!(
            card.game_has_capsule,
            "{case}: the session card has Steam art"
        );

        // NOW PLAYING pill.
        h.emit(game_detected("brawlhalla", "Brawlhalla", now_ms() - 60_000));
        assert!(
            h.app().get_game_has_capsule(),
            "{case}: the pill has Steam art"
        );
        assert!(!h.app().get_game_has_runtime_icon());
        assert_eq!(h.app().get_game_monogram().as_str(), "BR");
    }
}

/// A game with no icon and no Steam art is the monogram on the tile and on
/// the pill alike.
#[test]
fn a_game_without_art_is_the_monogram_on_tile_and_pill() {
    let mut h = Harness::new();
    h.app()
        .window()
        .set_size(slint::PhysicalSize::new(1600, 1000));
    logged_in_app(&mut h);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![recent_games_entry(serde_json::json!([
            { "game_id": "epic-zzunknown", "game_name": "Zz Unknown Game", "minutes": 0, "launcher": "epic" }
        ]))]),
    });
    let tile = first_card(&h).recent_games.row_data(0).expect("a tile");
    assert!(!tile.has_capsule && !tile.has_icon);
    h.emit(game_detected(
        "epic-zzunknown",
        "Zz Unknown Game",
        now_ms() - 60_000,
    ));
    assert!(!h.app().get_game_has_capsule() && !h.app().get_game_has_runtime_icon());
    assert_eq!(h.app().get_game_monogram().as_str(), tile.monogram.as_str());
    assert_eq!(h.app().get_game_hue_index(), tile.hue_index);
}

fn solo_crew_feed(h: &mut Harness) {
    app_with_crew_of(h, 1);
    set_roster(h, &[("user-1", "Test User")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            1,
            vec![h2h_title(
                "the_carry",
                "user-1",
                serde_json::json!(4167.0),
                "h",
            )],
            vec![],
            None,
        )]),
    });
}

/// With poster art the live card takes the wide slot and shows the poster
/// instead of the tile. The wide head2head card takes the wide slot of row 2.
#[test]
fn a_live_session_with_poster_art_is_wide_and_shows_the_poster() {
    let mut h = Harness::new();
    solo_crew_feed(&mut h);
    let _art = install_with_poster(&h, "291550", "Brawlhalla");
    h.emit(game_detected(
        "brawlhalla",
        "Brawlhalla",
        now_ms() - 12 * 60_000,
    ));

    let card = live_cards(&h).remove(0);
    assert!(card.live.has_poster, "the card carries the poster");
    assert_eq!(first_card(&h).card_type.as_str(), "live-session");
    let el: Vec<ElementHandle> =
        ElementHandle::find_by_element_type_name(h.app(), "LiveSessionCard").collect();
    assert_eq!(el.len(), 1);
    assert!(el[0].size().width > 420.0, "the wide slot");
    let (pos, size) = (el[0].absolute_position(), el[0].size());
    let tiles = ElementHandle::find_by_element_type_name(h.app(), "GameArt")
        .filter(|t| {
            let p = t.absolute_position();
            p.x >= pos.x && p.y >= pos.y && p.x < pos.x + size.width && p.y < pos.y + size.height
        })
        .count();
    assert_eq!(tiles, 0, "the poster is the art, there is no tile");
    assert_eq!(count_of(&h, "Head2headCardWide"), 1);
    assert_two_columns_wide(&h, "Head2headCardWide");
    let texts = drawn_texts(&h);
    for want in ["NOW PLAYING", "Brawlhalla", "since 12 min"] {
        assert!(texts.iter().any(|t| t == want), "{want} in {texts:?}");
    }
}

/// Without poster art the live card is a cell, and the wide head2head card of a
/// crew of one keeps the wide slot.
#[test]
fn a_live_session_without_poster_art_is_a_cell_and_head2head_keeps_the_wide_slot() {
    let mut h = Harness::new();
    solo_crew_feed(&mut h);
    h.emit(game_detected(
        "rocket-league",
        "Rocket League",
        now_ms() - 12 * 60_000,
    ));

    let card = live_cards(&h).remove(0);
    assert!(!card.live.has_poster);
    assert_eq!(count_of(&h, "Head2headCardWide"), 1, "the wide card stays");
    let el: Vec<ElementHandle> =
        ElementHandle::find_by_element_type_name(h.app(), "LiveSessionCard").collect();
    assert_eq!(el.len(), 1);
    assert!(el[0].size().width < 420.0, "a cell");
    let cards: Vec<_> = h.app().get_feed_cards().iter().collect();
    assert_eq!(cards[0].card_type.as_str(), "head2head");
    assert_eq!(cards[1].card_type.as_str(), "live-session");
}

fn live_cards(h: &Harness) -> Vec<FeedCardData> {
    h.app()
        .get_feed_cards()
        .iter()
        .filter(|c| c.card_type.as_str() == "live-session")
        .collect()
}

/// A game in progress for the viewer is a card with the red pip, the game and
/// "since N min", counted from the start the sensor reports.
#[test]
fn a_live_session_for_the_viewer_shows_the_pip_and_since_n_min() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![h2h_title(
                "the_carry",
                "user-1",
                serde_json::json!(4167.0),
                "h",
            )],
            vec![h2h_pair("u2", 1, 1)],
            None,
        )]),
    });
    assert_eq!(count_of(&h, "LiveSessionCard"), 0, "nothing plays");

    h.emit(game_detected(
        "counter-strike-2",
        "Counter-Strike 2",
        now_ms() - 12 * 60_000 - 5_000,
    ));

    assert_eq!(count_of(&h, "LiveSessionCard"), 1);
    assert_eq!(count_of(&h, "LivePip"), 1, "the one red pip");
    let texts = drawn_texts(&h);
    for want in ["NOW PLAYING", "Counter-Strike 2", "since 12 min"] {
        assert!(texts.iter().any(|t| t == want), "{want} in {texts:?}");
    }
    let card = live_cards(&h).remove(0);
    assert_eq!(card.live.since_min, 12);
    assert_eq!(card.live.players.row_count(), 1);
    let cards: Vec<_> = h.app().get_feed_cards().iter().collect();
    assert_eq!(cards[0].card_type.as_str(), "head2head");
    assert_eq!(
        cards[1].card_type.as_str(),
        "live-session",
        "no poster art, so it is the cell beside the lead"
    );
}

/// Two crewmates in one game are one card, with both octagons.
#[test]
fn two_crewmates_in_the_same_game_share_one_card() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 3);
    set_roster(
        &h,
        &[("user-1", "Test User"), ("u2", "Jonas"), ("u3", "Sara")],
    );
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            3,
            vec![h2h_title(
                "the_carry",
                "user-1",
                serde_json::json!(4167.0),
                "h",
            )],
            vec![h2h_pair("u2", 1, 1), h2h_pair("u3", 1, 1)],
            None,
        )]),
    });
    h.emit(mate_plays("u2", "rocket-league", "Rocket League", 20));
    h.emit(mate_plays("u3", "rocket-league", "Rocket League", 8));

    assert_eq!(count_of(&h, "LiveSessionCard"), 1, "one card for one game");
    let card = live_cards(&h).remove(0);
    assert_eq!(card.live.players.row_count(), 2);
    assert_eq!(card.live.names_line.as_str(), "Jonas and Sara");
    assert_eq!(card.live.since_min, 20, "counts from the earliest start");

    // A third game makes a second card. A cleared game leaves its card.
    h.emit(mate_plays("u3", "dota-2", "Dota 2", 3));
    assert_eq!(live_cards(&h).len(), 2);
    h.emit(Event::PresenceChanged {
        change: mello_core::crew_state::PresenceChange {
            crew_id: "crew-0".into(),
            user_id: "u3".into(),
            presence: mello_core::crew_state::PresenceInfo {
                status: mello_core::presence::PresenceStatus::Online,
                activity: None,
                game: None,
            },
        },
    });
    let left = live_cards(&h);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].live.names_line.as_str(), "Jonas");
}

/// A session started 3 min ago is on the feed from the first sensor report.
/// Nothing prunes a live card: the server curation only scores the ended
/// session card.
#[test]
fn a_live_session_started_3_min_ago_is_on_the_feed() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![h2h_title(
                "the_carry",
                "user-1",
                serde_json::json!(4167.0),
                "h",
            )],
            vec![h2h_pair("u2", 1, 1)],
            None,
        )]),
    });
    h.emit(game_detected(
        "counter-strike-2",
        "Counter-Strike 2",
        now_ms() - 3 * 60_000 - 5_000,
    ));
    assert_eq!(count_of(&h, "LiveSessionCard"), 1);
    assert_eq!(
        live_cards(&h).remove(0).live.since_text.as_str(),
        "since 3 min"
    );
}

/// The live card has the shape of the session card: a fixed game tile of the
/// session tile's size, in the wide slot and in a cell, and a card height that
/// equals the side cell.
#[test]
fn the_live_card_has_the_session_tile_size_in_a_cell_and_the_poster_in_the_wide_slot() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![h2h_title(
                "the_carry",
                "user-1",
                serde_json::json!(4167.0),
                "h",
            )],
            vec![h2h_pair("u2", 1, 1)],
            None,
        )]),
    });
    let _art = install_with_poster(&h, "730", "Counter-Strike 2");
    h.emit(game_detected(
        "counter-strike-2",
        "Counter-Strike 2",
        now_ms() - 12 * 60_000,
    ));
    h.emit(mate_plays("u2", "rocket-league", "Rocket League", 20));
    assert_eq!(count_of(&h, "LiveSessionCard"), 2);
    let cards: Vec<ElementHandle> =
        ElementHandle::find_by_element_type_name(h.app(), "LiveSessionCard").collect();
    let (wide, cell) = if cards[0].size().width > cards[1].size().width {
        (&cards[0], &cards[1])
    } else {
        (&cards[1], &cards[0])
    };
    assert!(wide.size().width > 420.0, "one card is in the wide slot");
    assert!(cell.size().width < 420.0, "one card is in a cell");
    assert_eq!(
        wide.size().height,
        cell.size().height,
        "the wide card is as high as the side cell"
    );
    for (card, want) in [(wide, 0), (cell, 1)] {
        let (pos, size) = (card.absolute_position(), card.size());
        let tiles: Vec<ElementHandle> =
            ElementHandle::find_by_element_type_name(h.app(), "GameArt")
                .filter(|t| {
                    let p = t.absolute_position();
                    p.x >= pos.x
                        && p.y >= pos.y
                        && p.x < pos.x + size.width
                        && p.y < pos.y + size.height
                })
                .collect();
        // The wide card has the poster, so it has no tile.
        assert_eq!(
            tiles.len(),
            want,
            "the tile of the cell, none on the poster"
        );
        if want == 1 {
            assert_eq!(tiles[0].size().width, 40.0, "the session tile width");
            assert_eq!(tiles[0].size().height, 40.0, "the session tile height");
        }
    }
}

/// A live session with poster art holds the wide slot of row 1, so the wide
/// head2head card of a crew of one takes the wide slot of row 2. It is two
/// columns wide before, during and after the game.
#[test]
fn the_wide_head2head_card_takes_row_two_while_a_live_session_holds_the_slot() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 1);
    set_roster(&h, &[("user-1", "Test User")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            1,
            vec![h2h_title(
                "the_carry",
                "user-1",
                serde_json::json!(4167.0),
                "h",
            )],
            vec![],
            None,
        )]),
    });
    assert_eq!(count_of(&h, "Head2headCardWide"), 1);

    let _art = install_with_poster(&h, "730", "Counter-Strike 2");
    h.emit(game_detected(
        "counter-strike-2",
        "Counter-Strike 2",
        now_ms(),
    ));
    assert_eq!(count_of(&h, "Head2headCardWide"), 1, "wide in row 2");
    assert_two_columns_wide(&h, "Head2headCardWide");
    assert_eq!(count_of(&h, "LiveSessionCard"), 1);
    let cards: Vec<_> = h.app().get_feed_cards().iter().collect();
    assert_eq!(cards[0].card_type.as_str(), "live-session");
    assert_eq!(cards[1].card_type.as_str(), "head2head");
    assert_eq!(
        cards[1].head2head.variant.as_str(),
        "wide",
        "the entry is the same, only the slot differs"
    );
    let live = ElementHandle::find_by_element_type_name(h.app(), "LiveSessionCard")
        .next()
        .expect("the live card");
    let h2h = ElementHandle::find_by_element_type_name(h.app(), "Head2headCardWide")
        .next()
        .expect("head2head");
    assert!(
        h2h.absolute_position().y >= live.absolute_position().y + live.size().height,
        "head2head is the row under the live card"
    );
    assert_eq!(h.controls_labelled("Invite your crew").len(), 1);

    h.emit(game_ended("counter-strike-2", "Counter-Strike 2", 1));
    assert_eq!(count_of(&h, "LiveSessionCard"), 0);
    assert_eq!(count_of(&h, "Head2headCardWide"), 1, "wide again");
    assert_two_columns_wide(&h, "Head2headCardWide");
}

/// The session end removes the live card, and the session card from the ledger
/// is the only card of that game. No duplicate.
#[test]
fn the_live_card_is_gone_after_the_session_end_and_the_session_card_is_there_once() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![h2h_title(
                "the_carry",
                "user-1",
                serde_json::json!(4167.0),
                "h",
            )],
            vec![h2h_pair("u2", 1, 1)],
            None,
        )]),
    });
    h.emit(game_detected(
        "rocket-league",
        "Rocket League",
        now_ms() - 30 * 60_000,
    ));
    assert_eq!(live_cards(&h).len(), 1);

    h.emit(game_ended("rocket-league", "Rocket League", 30));
    assert!(live_cards(&h).is_empty(), "the live card leaves at once");

    // The server pushes "feed changed" and the feed loads with the session.
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![
            h2h_entry(
                2,
                vec![h2h_title(
                    "the_carry",
                    "user-1",
                    serde_json::json!(4167.0),
                    "h",
                )],
                vec![h2h_pair("u2", 1, 1)],
                None,
            ),
            feed_entry(
                "gs-1",
                "session",
                serde_json::json!({
                    "game_name": "Rocket League", "game_id": "rocket-league",
                    "player_names": ["Test User"], "player_ids": ["user-1"],
                    "duration_min": 30, "wins": 0, "losses": 0, "draws": 0,
                }),
            ),
        ]),
    });
    assert_eq!(count_of(&h, "LiveSessionCard"), 0);
    assert_eq!(count_of(&h, "GameSessionCard"), 1, "one session card");
}

/// A fresh feed load keeps the live card: it is presence state, not a feed entry.
#[test]
fn a_feed_that_loads_while_a_game_runs_keeps_the_live_card() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(game_detected("rocket-league", "Rocket League", now_ms()));
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![h2h_title(
                "the_carry",
                "user-1",
                serde_json::json!(4167.0),
                "h",
            )],
            vec![h2h_pair("u2", 1, 1)],
            None,
        )]),
    });
    assert_eq!(live_cards(&h).len(), 1);
    assert_eq!(count_of(&h, "LiveSessionCard"), 1);
}

// ── Quiet and dormant feeds (mockup frames E and F) ─────────────────────────

const DAY_MS: i64 = 86_400_000;

fn entry_ago(id: &str, entry_type: &str, data: serde_json::Value, ago_ms: i64) -> FeedEntry {
    let mut e = feed_entry(id, entry_type, data);
    e.ts = now_ms() - ago_ms;
    e
}

fn solo_session(id: &str, player: &str, player_id: &str, ago_days: i64) -> FeedEntry {
    entry_ago(
        id,
        "session",
        serde_json::json!({
            "game_name": "Counter-Strike 2", "player_names": [player], "player_ids": [player_id],
            "duration_min": 160, "wins": 3, "losses": 2, "draws": 0,
        }),
        ago_days * DAY_MS,
    )
}

/// A title that changed hands `days` ago, with the pair that says both values.
fn belt_entry(days: i64) -> FeedEntry {
    let moved_at = now_ms() - days * DAY_MS - 3_600_000;
    let night_owl = serde_json::json!({
        "title_id": "night_owl", "holder_user_id": "u4", "value": "03:40", "unit": "",
        "since_ms": moved_at, "moved_at_ms": moved_at, "previous_holder_user_id": "user-1",
    });
    feed_entry(
        "head2head",
        "head2head",
        serde_json::json!({
            "crew_id": "crew-0", "member_count": 4, "open_seats": 1,
            "titles": [
                h2h_title_held("the_carry", "u2", serde_json::json!(4201.0), "h", 9),
                night_owl,
            ],
            "pairs": [
                h2h_pair_values("u2", 4, 3, vec![("the_carry", serde_json::json!(4167.0), serde_json::json!(4201.0))]),
                h2h_pair_values("u3", 2, 5, vec![]),
                h2h_pair_values("u4", 3, 4, vec![("night_owl", serde_json::json!("03:14"), serde_json::json!("03:40"))]),
            ],
            "updated_at": 1_000,
        }),
    )
}

fn quiet_roster(h: &Harness) {
    set_roster(
        h,
        &[
            ("user-1", "Test User"),
            ("u2", "Jonas"),
            ("u3", "Max"),
            ("u4", "Sara"),
        ],
    );
}

fn card_types(h: &Harness) -> Vec<String> {
    let feed = h.app().get_feed_cards();
    (0..feed.row_count())
        .map(|i| {
            let c = feed.row_data(i).unwrap();
            if c.card_type.as_str() == "head2head" {
                format!("head2head:{}", c.head2head.variant)
            } else {
                c.card_type.to_string()
            }
        })
        .collect()
}

// --- stage 6 (streaming-quality): the stream session states on the cards ---

/// `(state, subtitle, title)` of each stream card, in order.
fn stream_cards(h: &Harness) -> Vec<(String, String, String)> {
    h.app()
        .get_active_stream_cards()
        .iter()
        .map(|c| {
            (
                c.state.to_string(),
                c.subtitle.to_string(),
                c.title.to_string(),
            )
        })
        .collect()
}

/// The host card follows the stream session: STARTING until the track is
/// open, LIVE, RECONNECTING while the link is silent, LIVE again; a stream
/// that ends on a lost connection keeps a card that says so.
#[test]
fn the_host_card_follows_the_stream_session() {
    let mut h = Harness::new();
    h.app().set_user_name("b0bben".into());

    h.emit(Event::StreamStarting {
        crew_id: "crew-1".into(),
    });
    assert!(
        h.app().get_is_hosting(),
        "END STREAM is there during the start"
    );
    assert_eq!(stream_cards(&h)[0].0, "starting");

    h.emit(Event::StreamStarted {
        crew_id: "crew-1".into(),
        session_id: "s1".into(),
        mode: "sfu".into(),
    });
    assert_eq!(stream_cards(&h)[0].0, "live");

    h.emit(Event::StreamReconnecting {
        role: mello_core::events::StreamRole::Host,
    });
    assert_eq!(stream_cards(&h)[0].0, "reconnecting");
    h.emit(Event::StreamResumed {
        role: mello_core::events::StreamRole::Host,
    });
    assert_eq!(stream_cards(&h)[0].0, "live");

    h.emit(Event::StreamEnded {
        crew_id: "crew-1".into(),
        role: mello_core::events::StreamRole::Host,
        reason: "connection_lost".into(),
    });
    assert!(!h.app().get_is_hosting());
    assert_eq!(
        stream_cards(&h),
        [(
            "ended".to_string(),
            "The connection to the stream was lost for 30 seconds.".to_string(),
            "STREAM ENDED".to_string()
        )]
    );
}

/// END STREAM ends without a notice card: the user ended it.
#[test]
fn end_stream_leaves_no_notice_card() {
    let mut h = Harness::new();
    h.emit(Event::StreamStarted {
        crew_id: "crew-1".into(),
        session_id: "s1".into(),
        mode: "sfu".into(),
    });
    h.emit(Event::StreamEnded {
        crew_id: "crew-1".into(),
        role: mello_core::events::StreamRole::Host,
        reason: "stopped".into(),
    });
    assert!(stream_cards(&h).is_empty(), "{:?}", stream_cards(&h));
}

/// A start that fails names the cause on a card; the host is not hosting.
#[test]
fn a_failed_start_leaves_a_card_that_names_the_cause() {
    let mut h = Harness::new();
    h.emit(Event::StreamStarting {
        crew_id: "crew-1".into(),
    });
    h.emit(Event::StreamFailed {
        role: mello_core::events::StreamRole::Host,
        reason: "Could not reach the stream server.".into(),
    });
    assert!(!h.app().get_is_hosting());
    assert_eq!(
        stream_cards(&h),
        [(
            "failed".to_string(),
            "Could not reach the stream server.".to_string(),
            "STREAM DID NOT START".to_string()
        )]
    );
}

/// The watched card shows RECONNECTING while the viewer reconnects, and
/// stays the watched card (the last frame stays under the wash).
#[test]
fn the_watched_card_shows_reconnecting_and_resumes() {
    let mut h = Harness::new();
    h.app().set_active_streamer_id("host-1".into());
    h.app().set_active_streamer_name("alice".into());
    h.app().set_active_stream_session_id("s1".into());
    h.emit(Event::StreamWatching {
        host_id: "host-1".into(),
        width: 1280,
        height: 720,
    });
    assert_eq!(stream_cards(&h)[0].0, "");

    h.emit(Event::StreamReconnecting {
        role: mello_core::events::StreamRole::Viewer,
    });
    assert!(h.app().get_is_watching(), "a reconnect is not a stop");
    assert_eq!(stream_cards(&h)[0].0, "reconnecting");

    h.emit(Event::StreamResumed {
        role: mello_core::events::StreamRole::Viewer,
    });
    assert_eq!(stream_cards(&h)[0].0, "");
}

// --- stage 9 (streaming-quality): the stream card's buttons and labels ---

/// The pop-out window as the UI harness sees it: it records what the card
/// asked, with the window title.
#[derive(Default)]
struct FakePopout {
    calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl crate::stream_popout::PopoutSurface for FakePopout {
    fn open(&mut self, _ctx: &crate::app_context::AppContext, title: &str) -> Result<(), String> {
        self.calls.borrow_mut().push(format!("open {title}"));
        Ok(())
    }

    fn close(&mut self, _ctx: &crate::app_context::AppContext) {
        self.calls.borrow_mut().push("close".into());
    }
}

type PopoutCalls = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

/// The app with alice's stream in the crew, not watched yet, and a fake
/// pop-out window. Returns what the fake records.
fn app_with_alices_stream(h: &mut Harness) -> PopoutCalls {
    use mello_core::crew_state::{CrewState, StreamState};
    logged_in_app(h);
    // The card lays its footer out only after the window has a size: the
    // controls on its right are absent from queries before.
    h.app()
        .window()
        .set_size(slint::PhysicalSize::new(1440, 900));
    h.app().set_user_id("user-1".into());
    h.app().set_active_crew_id("crew-1".into());
    h.emit(Event::CrewStateLoaded {
        state: CrewState {
            crew_id: "crew-1".into(),
            name: "Crew".into(),
            stream: Some(StreamState {
                active: true,
                stream_id: Some("s1".into()),
                streamer_id: Some("host-1".into()),
                streamer_username: Some("alice".into()),
                title: Some("Valorant".into()),
                viewer_count: 2,
                width: 1280,
                height: 720,
                ..Default::default()
            }),
            ..Default::default()
        },
    });
    let fake = FakePopout::default();
    let calls = fake.calls.clone();
    h.ctx()
        .stream_popout
        .borrow_mut()
        .set_surface(Box::new(fake));
    calls
}

/// Click Watch on alice's card and let the decoder come up.
fn watch_alice(h: &mut Harness) {
    h.click_label("Watch alice");
    h.emit(Event::StreamWatching {
        host_id: "host-1".into(),
        width: 1280,
        height: 720,
    });
}

fn stream_volumes(h: &mut Harness) -> Vec<f32> {
    h.commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::SetStreamAudioVolume { gain } => Some(gain),
            _ => None,
        })
        .collect()
}

/// Frame E: nothing live, no stream or belt in 24 h, no recap this week. The
/// quiet row says so, and the belt that moved two days ago leads the feed with
/// the head2head card, two columns wide, under it.
#[test]
fn a_quiet_feed_has_a_quiet_row_and_the_belt_that_moved_leads() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 4);
    quiet_roster(&h);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![
            belt_entry(2),
            solo_session("gs-1", "Test User", "user-1", 3),
            solo_session("gs-2", "Sara", "u4", 2),
        ]),
    });

    let quiet = h.app().get_feed_quiet();
    assert_eq!(quiet.row_count(), 1);
    let row = quiet.row_data(0).unwrap();
    assert_eq!(
        row.line.as_str(),
        "No crew time last week. Sara and Test User still played."
    );
    assert_eq!(row.when.as_str(), "", "the quiet row carries no note");
    assert_eq!(count_of(&h, "FeedJoinedRow"), 1, "the row is drawn");
    assert_eq!(h.app().get_feed_dormant().row_count(), 0);

    let types = card_types(&h);
    assert_eq!(
        types[..2],
        ["head2head:belt", "head2head:pairs"],
        "{types:?}"
    );
    let belt = first_card(&h).head2head.belt;
    assert_eq!(belt.headline.as_str(), "Sara took Night Owl from Test User");
    assert_eq!(belt.detail.as_str(), "03:40 beats 03:14.");
    assert_eq!(belt.age.as_str(), "2 days ago");
    assert_eq!(count_of(&h, "BeltHero"), 1);
    assert_eq!(skeleton_cells(&h), 0, "no skeletons from three members");
    // The cell stands tall, so it draws three pair rows beside the seat.
    let cell = h.app().get_feed_cards().row_data(1).unwrap();
    assert_eq!(cell.head2head.pairs.row_count(), 3);
    assert_eq!(cell.head2head.open_seats, 1);
}

/// A recap this week is the news. The quiet row stays off.
#[test]
fn a_recap_this_week_ends_the_quiet() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 4);
    quiet_roster(&h);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![
            belt_entry(2),
            entry_ago(
                "recap",
                "recap",
                serde_json::json!({
                    "week_end": now_ms() - DAY_MS, "total_hangout_min": 600, "clip_count": 3,
                    "top_members": [], "top_game": "Valorant",
                }),
                DAY_MS,
            ),
            solo_session("gs-1", "Sara", "u4", 2),
        ]),
    });
    assert_eq!(h.app().get_feed_quiet().row_count(), 0);
    assert_eq!(count_of(&h, "FeedJoinedRow"), 0);
}

/// A belt that moved in the last 24 hours is not quiet: the feed has news.
#[test]
fn a_belt_that_moved_today_ends_the_quiet() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 4);
    quiet_roster(&h);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![belt_entry(0), solo_session("gs-1", "Sara", "u4", 2)]),
    });
    assert_eq!(h.app().get_feed_quiet().row_count(), 0);
}

/// Frame F: nobody opened m3llo for three weeks. A strip says since when and
/// who holds a belt. The feed keeps the last real recap, labelled with its age,
/// and the two last sessions. The recap and the cell are the two tall cards.
#[test]
fn a_dormant_feed_keeps_the_last_recap_with_its_age_and_adds_a_strip() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 4);
    quiet_roster(&h);
    let recap_end = now_ms() - 21 * DAY_MS;
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![
            entry_ago(
                "recap",
                "recap",
                serde_json::json!({
                    "week_end": recap_end, "total_hangout_min": 375, "clip_count": 3,
                    "top_members": [], "top_game": "Rocket League", "longest_session_min": 140,
                    "belts_held": [{"user_id": "u4", "display_name": "Sara", "count": 3, "title_ids": ["night_owl"]}],
                }),
                21 * DAY_MS,
            ),
            solo_session("gs-1", "Test User", "user-1", 24),
            {
                // The belts are 24 days old and moved the day of the last session.
                let mut e = belt_entry(24);
                e.id = "head2head".into();
                e
            },
            solo_session("gs-2", "Sara", "u4", 27),
        ]),
    });

    let strip = h.app().get_feed_dormant();
    assert_eq!(strip.row_count(), 1);
    let card = strip.row_data(0).unwrap();
    assert_eq!(
        card.title.as_str(),
        format!(
            "Nothing new since {}. Jonas holds The Carry.",
            crate::feed_age::date_label(now_ms() - 24 * DAY_MS)
        )
    );
    assert_eq!(card.timestamp.as_str(), "3 weeks ago");
    assert_eq!(
        h.app().get_feed_quiet().row_count(),
        0,
        "the strip stands in"
    );

    let feed = h.app().get_feed_cards();
    let recap = (0..feed.row_count())
        .filter_map(|i| feed.row_data(i))
        .find(|c| c.card_type.as_str() == "recap")
        .expect("the last recap stays");
    assert_eq!(recap.age_label.as_str(), "3 weeks ago");
    let types = card_types(&h);
    // Head2head leads, the recap stands beside it (tall), two cells under it.
    assert_eq!(types[0], "head2head:pairs", "{types:?}");
    assert_eq!(types[1], "recap", "{types:?}");
    assert_eq!(skeleton_cells(&h), 0, "no skeletons");
    assert_eq!(count_of(&h, "RecapCard"), 1);
}

// ---------------------------------------------------------------------------
// Daily recap (CLIPS 7.1)
// ---------------------------------------------------------------------------

/// A `daily-recap` entry for the UTC day `days_ago` days back, as the server
/// builds it. `extra` overrides fields.
fn daily_entry(days_ago: i64, extra: serde_json::Value) -> FeedEntry {
    let day_start = (now_ms().div_euclid(DAY_MS) - days_ago) * DAY_MS;
    let mut data = serde_json::json!({
        "day_start": day_start, "day_end": day_start + DAY_MS,
        "total_hangout_min": 30, "total_game_min": 95, "total_min": 125,
        "session_count": 3, "clip_count": 1,
        "top_game": "Rocket League", "game_id": "rocket-league",
        "member_ids": ["user-1", "u2"],
    });
    for (k, v) in extra.as_object().expect("object").iter() {
        data[k] = v.clone();
    }
    feed_entry(&format!("daily_{days_ago}"), "daily-recap", data)
}

fn daily_card(h: &Harness) -> Option<FeedCardData> {
    let feed = h.app().get_feed_cards();
    (0..feed.row_count())
        .filter_map(|i| feed.row_data(i))
        .find(|c| c.card_type.as_str() == "daily-recap")
}

/// The card states the day, the four numbers, the members and the belts.
#[test]
fn the_daily_recap_card_shows_the_day_the_numbers_the_members_and_two_belts() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![daily_entry(
            0,
            serde_json::json!({
                "belts_moved": [
                    belt_moved("the_carry", serde_json::json!(4201.0), "h"),
                    belt_moved("night_owl", serde_json::json!("03:40"), ""),
                    belt_moved("marathon", serde_json::json!("3 h 48"), ""),
                ],
            }),
        )]),
    });

    let card = daily_card(&h).expect("a daily recap card");
    assert_eq!(card.day_label.as_str(), "Today so far");
    assert_eq!(card.title.as_str(), "2h 5m", "the hangout time");
    assert_eq!(card.clip_count, 1);
    assert_eq!(card.day_sessions, 3);
    assert_eq!(card.game_name.as_str(), "Rocket League");
    assert_eq!(card.day_members.row_count(), 2);
    assert_eq!(card.belts_moved.row_count(), 2, "two belts at most");
    assert_eq!(count_of(&h, "DailyRecapCard"), 1);
    let texts = drawn_texts(&h);
    for want in [
        "DAILY RECAP",
        "Today so far",
        "HANGOUT",
        "CLIPS",
        "SESSIONS",
        "TOP GAME",
        "Rocket League",
    ] {
        assert!(texts.iter().any(|t| t == want), "missing {want}: {texts:?}");
    }
    assert_eq!(count_of(&h, "TitleChip"), 2, "a chip for each belt");
}

/// The day word: "Today so far", "Yesterday", then the day name.
#[test]
fn the_daily_recap_names_its_day() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    for (days_ago, want) in [(0, "Today so far"), (1, "Yesterday")] {
        h.emit(Event::FeedLoaded {
            response: feed_of(vec![daily_entry(days_ago, serde_json::json!({}))]),
        });
        assert_eq!(daily_card(&h).expect("card").day_label.as_str(), want);
    }
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![daily_entry(2, serde_json::json!({}))]),
    });
    let day_start = (now_ms().div_euclid(DAY_MS) - 2) * DAY_MS;
    assert_eq!(
        daily_card(&h).expect("card").day_label.as_str(),
        crate::feed_age::weekday_of(day_start)
    );
}

/// The server sends up to three days. The feed draws one daily recap, the
/// newest.
#[test]
fn only_the_newest_daily_recap_is_drawn() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![
            daily_entry(0, serde_json::json!({})),
            daily_entry(1, serde_json::json!({})),
            daily_entry(2, serde_json::json!({})),
        ]),
    });
    let types = card_types(&h);
    assert_eq!(types.iter().filter(|t| *t == "daily-recap").count(), 1);
    assert_eq!(
        daily_card(&h).expect("card").day_label.as_str(),
        "Today so far"
    );
    assert_eq!(count_of(&h, "DailyRecapCard"), 1);
}

/// A day with nothing has no entry, so the feed has no card for it.
#[test]
fn a_feed_with_no_daily_recap_draws_none() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![solo_session("gs-1", "Test User", "user-1", 5)]),
    });
    assert!(daily_card(&h).is_none());
    assert_eq!(count_of(&h, "DailyRecapCard"), 0);
}

/// An older server still sends catch-up cards. They are not drawn and do not
/// break the feed.
#[test]
fn a_catchup_entry_from_an_older_server_is_not_drawn() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![
            feed_entry(
                "evt-join-1",
                "catchup",
                serde_json::json!({ "username": "kim", "display_name": "Kim" }),
            ),
            daily_entry(0, serde_json::json!({})),
        ]),
    });
    let types = card_types(&h);
    assert!(!types.iter().any(|t| t == "catchup"), "{types:?}");
    assert_eq!(types.iter().filter(|t| *t == "daily-recap").count(), 1);
    let texts = drawn_texts(&h);
    assert!(!texts.iter().any(|t| t == "CATCH-UP"), "{texts:?}");
}

fn card_ui(h: &Harness) -> crate::StreamCardUi<'_> {
    h.app().global::<crate::StreamCardUi>()
}

fn timeline(frames_released: u64) -> Event {
    Event::StreamViewerTimeline {
        target_delay_ms: 118.4,
        jitter_ms: 6.2,
        decode_ms_p95: 2.06,
        frames_released,
        frames_late: 0,
        frames_dropped_late: 0,
        decoder: "NVDEC".into(),
        decode_fps: 60.0,
        audio_delay_ms: 141.6,
        av_offset_ms: 8.4,
        audio_expand_ms: 0,
        audio_fec_used: 0,
    }
}

/// ★ Watch: the click says STARTING until the first frame; the watched card
/// then has the three buttons and the viewer count. Before stage 9 the
/// buttons had no handler and Maximize had no label.
#[test]
fn watching_a_stream_shows_starting_then_the_cards_three_buttons() {
    let mut h = Harness::new();
    app_with_alices_stream(&mut h);
    assert!(
        h.controls_labelled("Stream volume").is_empty()
            && h.controls_labelled("Expand").is_empty()
            && h.controls_labelled("Pop out").is_empty(),
        "a card you do not watch has nothing to control"
    );

    h.click_label("Watch alice");
    assert!(
        h.commands()
            .iter()
            .any(|c| matches!(c, Command::WatchStream { host_id, .. } if host_id == "host-1")),
        "Watch asks core to watch"
    );
    assert!(card_ui(&h).get_viewer_starting(), "STARTING from the click");
    assert!(
        h.controls_labelled("Watch alice").is_empty(),
        "no second Watch while it starts"
    );

    h.emit(Event::StreamWatching {
        host_id: "host-1".into(),
        width: 1280,
        height: 720,
    });
    assert!(card_ui(&h).get_viewer_starting(), "no frame shows yet");
    for label in [
        "Stream volume",
        "Stream settings",
        "Expand",
        "Pop out",
        "Leave",
    ] {
        assert!(
            !h.controls_labelled(label).is_empty(),
            "the watched card has {label:?}"
        );
    }

    h.emit(timeline(1));
    assert!(
        !card_ui(&h).get_viewer_starting(),
        "the first frame ends it"
    );

    h.emit(Event::StreamMembersChanged {
        members: vec![
            mello_core::events::StreamMember {
                user_id: "host-1".into(),
                role: "host".into(),
            },
            mello_core::events::StreamMember {
                user_id: "bob".into(),
                role: "viewer".into(),
            },
            mello_core::events::StreamMember {
                user_id: "carol".into(),
                role: "viewer".into(),
            },
        ],
    });
    assert_eq!(
        h.app()
            .get_active_stream_cards()
            .row_data(0)
            .expect("the watched card")
            .viewer_count,
        3,
        "bob, carol and you watch"
    );
}

/// ★ Volume: the popover sets the stream's game-audio gain; the mute keeps
/// the volume; the value comes back when you watch the same host again.
#[test]
fn the_stream_volume_popover_sets_mutes_and_remembers_the_host() {
    let mut h = Harness::new();
    app_with_alices_stream(&mut h);
    watch_alice(&mut h);
    assert!(
        stream_volumes(&mut h).is_empty(),
        "unity needs no command: a new view starts at unity"
    );

    h.click_label("Stream volume");
    assert_eq!(card_ui(&h).get_popover(), "volume");
    let slider = h
        .controls_labelled("Stream volume for alice")
        .into_iter()
        .find(|e| e.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Slider))
        .expect("the popover's slider");
    slider.set_accessible_value("70");
    h.pump();
    assert_eq!(stream_volumes(&mut h), vec![0.7]);
    assert_eq!(card_ui(&h).get_volume(), 70, "the popover shows it");

    h.click_label("Mute the stream");
    assert_eq!(stream_volumes(&mut h), vec![0.0]);
    assert!(card_ui(&h).get_muted());
    assert_eq!(card_ui(&h).get_volume(), 70, "the volume stays");

    press_escape(&mut h);
    assert_eq!(card_ui(&h).get_popover(), "", "Escape closes it");

    h.click_label("Leave");
    assert!(h
        .commands()
        .iter()
        .any(|c| matches!(c, Command::StopWatching)));
    h.emit(Event::StreamWatchingStopped);
    watch_alice(&mut h);
    assert_eq!(
        stream_volumes(&mut h),
        vec![0.0],
        "the new view gets the muted stream of alice at once"
    );

    h.click_label("Stream volume");
    h.click_label("Mute the stream");
    h.click_label("Reset stream volume to 100%");
    assert_eq!(stream_volumes(&mut h), vec![0.7, 1.0]);
}

/// ★ Settings: a viewer turns the stats strip on; it shows the 1 s
/// timeline values. The host sees the quality and the send rate, and its
/// viewers.
#[test]
fn stream_settings_show_the_stats_for_a_viewer_and_the_quality_for_the_host() {
    let mut h = Harness::new();
    app_with_alices_stream(&mut h);
    watch_alice(&mut h);

    h.click_label("Stream settings");
    assert_eq!(card_ui(&h).get_popover(), "settings");
    h.click_label("Show stream stats");
    assert!(card_ui(&h).get_stats_shown());
    h.emit(timeline(60));
    let stats = card_ui(&h).get_stats();
    assert_eq!(
        (
            stats.target_delay.as_str(),
            stats.jitter.as_str(),
            stats.decode.as_str(),
            stats.audio_delay.as_str(),
            stats.av_offset.as_str()
        ),
        ("118 ms", "6 ms", "2.1 ms", "142 ms", "+8 ms")
    );
    assert!(
        visible_texts(&h).iter().any(|t| t == "118 ms"),
        "the strip shows on the card"
    );

    let mut host = Harness::new();
    logged_in_app(&mut host);
    host.app()
        .window()
        .set_size(slint::PhysicalSize::new(1440, 900));
    host.app().set_stream_preset(1);
    host.emit(Event::StreamStarting {
        crew_id: "crew-0".into(),
    });
    host.emit(Event::StreamStarted {
        crew_id: "crew-0".into(),
        session_id: "s1".into(),
        mode: "sfu".into(),
    });
    host.emit(Event::StreamHostPacingStats {
        mode: "sfu".into(),
        target_kbps: 8000,
        out_kbps: 6200.0,
        paced_bytes: 0,
        sleep_count: 0,
        sleep_ms_total: 0,
        sleep_count_delta: 0,
        sleep_ms_delta: 0,
    });
    assert!(
        host.controls_labelled("Stream volume").is_empty(),
        "the host hears no game audio of its own stream"
    );
    host.click_label("Stream settings");
    assert_eq!(card_ui(&host).get_popover(), "host-settings");
    assert_eq!(card_ui(&host).get_host_quality(), "HIGH \u{b7} 1080p30");
    assert_eq!(card_ui(&host).get_host_sending(), "6.2 Mbps");

    host.emit(Event::StreamViewerJoined {
        viewer_id: "bob".into(),
    });
    assert_eq!(
        host.app()
            .get_active_stream_cards()
            .row_data(0)
            .expect("the hosted card")
            .viewer_count,
        1
    );
    host.emit(Event::StreamEnded {
        crew_id: "crew-0".into(),
        role: mello_core::events::StreamRole::Host,
        reason: "stopped".into(),
    });
    assert_eq!(
        card_ui(&host).get_popover(),
        "",
        "the popover goes with its card"
    );
}

fn no_restart(h: &mut Harness) {
    let cmds = h.commands();
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::WatchStream { .. } | Command::StopWatching)),
        "the switch restarts nothing: {cmds:?}"
    );
    assert!(h.app().get_is_watching());
}

/// Whether the expanded view is on screen (main.slint over the feed).
fn expanded_view_shows(h: &Harness) -> bool {
    ElementHandle::find_by_element_type_name(h.app(), "ExpandedStreamCard")
        .next()
        .is_some()
}

/// ★ Expand: the watched card takes over the feed area and keeps its
/// footer; the same button, now "Exit expanded view", and Escape bring it
/// back; nothing restarts. A pause and a leave return to the normal card.
#[test]
fn expand_takes_over_the_feed_and_exit_escape_pause_and_leave_return() {
    let mut h = Harness::new();
    app_with_alices_stream(&mut h);
    watch_alice(&mut h);
    let _ = h.commands();

    h.click_label("Expand");
    assert!(card_ui(&h).get_expanded());
    assert!(expanded_view_shows(&h), "the card covers the feed area");
    assert!(
        !h.controls_labelled("Exit expanded view").is_empty()
            && !h.controls_labelled("Leave").is_empty(),
        "the footer and its buttons stay"
    );
    no_restart(&mut h);

    h.click_label("Exit expanded view");
    assert!(!card_ui(&h).get_expanded());
    assert!(!expanded_view_shows(&h));
    no_restart(&mut h);

    h.click_label("Expand");
    press_escape(&mut h);
    assert!(!card_ui(&h).get_expanded(), "Escape returns");

    h.click_label("Expand");
    h.emit(Event::StreamPaused {
        host_id: "host-1".into(),
        paused: true,
        reason: "minimized".into(),
    });
    assert!(!card_ui(&h).get_expanded(), "a pause shows the normal card");
    assert!(
        h.controls_labelled("Expand").is_empty(),
        "a paused stream has no picture to expand"
    );
    h.emit(Event::StreamPaused {
        host_id: "host-1".into(),
        paused: false,
        reason: "".into(),
    });

    h.click_label("Expand");
    h.click_label("Leave");
    h.emit(Event::StreamWatchingStopped);
    assert!(!card_ui(&h).get_expanded(), "a leave ends it");
    assert!(!expanded_view_shows(&h));
}

/// ★ Pop out: the stream opens in its own window titled after the host; the
/// card says "Playing in a window" and its button brings it back; nothing
/// restarts. Closing the window, a pause, the end and a leave bring it back
/// too. The pop-out and the expanded view exclude each other.
#[test]
fn pop_out_opens_a_window_and_close_pause_end_and_leave_bring_it_back() {
    let mut h = Harness::new();
    let calls = app_with_alices_stream(&mut h);
    watch_alice(&mut h);
    let _ = h.commands();

    h.click_label("Pop out");
    assert_eq!(*calls.borrow(), ["open alice - Mello"]);
    assert!(card_ui(&h).get_popped_out());
    assert!(
        !h.controls_labelled("Show it here").is_empty(),
        "the card says the stream is in a window and brings it back"
    );
    assert!(
        !h.controls_labelled("Stream volume").is_empty()
            && !h.controls_labelled("Stream settings").is_empty(),
        "the volume and the settings stay on the card"
    );
    no_restart(&mut h);

    h.click_label("Show it here");
    assert_eq!(*calls.borrow(), ["open alice - Mello", "close"]);
    assert!(!card_ui(&h).get_popped_out());

    // The window's close button: the native window procedure calls this.
    h.click_label("Pop out");
    crate::stream_popout::close(h.ctx(), "the window closed");
    assert!(!card_ui(&h).get_popped_out());
    assert!(h.app().get_is_watching(), "the stream plays on in the card");

    // Exclusive: expanding brings the stream back from the window; popping
    // out leaves the expanded view.
    h.click_label("Pop out");
    h.click_label("Expand");
    assert!(card_ui(&h).get_expanded() && !card_ui(&h).get_popped_out());
    h.click_label("Pop out");
    assert!(!card_ui(&h).get_expanded() && card_ui(&h).get_popped_out());

    h.emit(Event::StreamPaused {
        host_id: "host-1".into(),
        paused: true,
        reason: "minimized".into(),
    });
    assert!(!card_ui(&h).get_popped_out(), "a pause brings it back");
    h.emit(Event::StreamPaused {
        host_id: "host-1".into(),
        paused: false,
        reason: "".into(),
    });

    h.click_label("Pop out");
    h.emit(Event::StreamFailed {
        role: mello_core::events::StreamRole::Viewer,
        reason: "The stream server closed the session.".into(),
    });
    assert!(!card_ui(&h).get_popped_out(), "a failure brings it back");

    h.click_label("Pop out");
    h.click_label("Leave");
    h.emit(Event::StreamWatchingStopped);
    assert!(!card_ui(&h).get_popped_out(), "a leave brings it back");
    let closes = calls.borrow().iter().filter(|c| *c == "close").count();
    let opens = calls.borrow().len() - closes;
    assert_eq!(opens, closes, "every window closed: {:?}", calls.borrow());
}

/// ★ Reconnect: the watched card keeps its buttons through a reconnect, and
/// its popover and label go with the stream.
#[test]
fn the_watched_card_keeps_its_buttons_through_a_reconnect_and_clears_on_leave() {
    let mut h = Harness::new();
    app_with_alices_stream(&mut h);
    watch_alice(&mut h);
    card_ui(&h).set_format_label("720p60".into());

    h.emit(Event::StreamReconnecting {
        role: mello_core::events::StreamRole::Viewer,
    });
    assert_eq!(stream_cards(&h)[0].0, "reconnecting");
    assert!(!h.controls_labelled("Stream volume").is_empty());
    h.emit(Event::StreamResumed {
        role: mello_core::events::StreamRole::Viewer,
    });

    h.click_label("Stream volume");
    h.emit(Event::StreamEnded {
        crew_id: "crew-1".into(),
        role: mello_core::events::StreamRole::Viewer,
        reason: "host_left".into(),
    });
    h.emit(Event::StreamWatchingStopped);
    assert_eq!(card_ui(&h).get_popover(), "", "the popover goes");
    assert_eq!(card_ui(&h).get_format_label(), "", "the label goes");
    assert!(h.controls_labelled("Stream volume").is_empty());
}

/// The card's format label: the rate of the rung message and the decoded
/// height; without a decoded picture the height of the rung.
#[test]
fn the_format_label_follows_the_rung_and_the_decoded_size() {
    use mello_core::stream::controller::{rung_message, Rung};
    use mello_core::stream::viewer_format;
    let h = Harness::new();
    viewer_format::reset_viewer_format();
    crate::handlers::streaming::refresh_stream_format(h.app(), 0);
    assert_eq!(card_ui(&h).get_format_label(), "");

    viewer_format::apply_viewer_rung_message(&rung_message(Rung {
        width: 854,
        height: 480,
        fps: 30,
    }));
    crate::handlers::streaming::refresh_stream_format(h.app(), 0);
    assert_eq!(card_ui(&h).get_format_label(), "480p30");
    crate::handlers::streaming::refresh_stream_format(h.app(), 720);
    assert_eq!(
        card_ui(&h).get_format_label(),
        "720p30",
        "the SPS wins for the height"
    );
    viewer_format::reset_viewer_format();
}

mod sealed;

// ---- the recap is never a one-cell card ----

/// `(width, height)` of the first element of a component type.
fn size_of_type(h: &Harness, name: &str) -> (f32, f32) {
    let found: Vec<_> = ElementHandle::find_by_element_type_name(h.app(), name).collect();
    assert!(!found.is_empty(), "no {name} on screen");
    let s = found[0].size();
    (s.width, s.height)
}

/// The card of this type is two grid columns wide: more than half the feed
/// panel, where a one-column cell is a third of it.
fn assert_two_columns_wide(h: &Harness, name: &str) {
    let (w, _) = size_of_type(h, name);
    let (feed, _) = size_of_type(h, "CrewFeed");
    assert!(
        w > feed * 0.5,
        "{name} is {w} px wide in a {feed} px feed: it is not two columns wide"
    );
}

/// The smallest width and the smallest height among the elements of a type:
/// the size of one grid cell, whichever slot the first one stands in.
fn cell_size_of_type(h: &Harness, name: &str) -> (f32, f32) {
    let found: Vec<_> = ElementHandle::find_by_element_type_name(h.app(), name).collect();
    assert!(!found.is_empty(), "no {name} on screen");
    found.iter().fold((f32::MAX, f32::MAX), |(w, h), e| {
        (w.min(e.size().width), h.min(e.size().height))
    })
}

/// A crew of five with a weekly recap that has belts, head2head, a daily
/// recap and sessions, in the given server order.
fn crew_with_recap_h2h_daily_and_sessions(order: &[&str]) -> Harness {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 5);
    set_roster(
        &h,
        &[
            ("user-1", "Test User"),
            ("u2", "Jonas"),
            ("u3", "Sara"),
            ("u4", "Max"),
            ("u5", "Leo"),
        ],
    );
    let entry = |kind: &str| -> FeedEntry {
        match kind {
            "recap" => recap_entry(serde_json::json!({
                "belts_moved": [belt_moved("the_carry", serde_json::json!(4201.0), "h")],
                "belts_held": belts_held(),
            })),
            "h2h" => h2h_entry(
                5,
                vec![h2h_title("the_carry", "u2", serde_json::json!(4201.0), "h")],
                vec![
                    h2h_pair("u2", 1, 3),
                    h2h_pair("u3", 2, 2),
                    h2h_pair("u4", 3, 1),
                    h2h_pair("u5", 2, 2),
                ],
                None,
            ),
            "daily" => feed_entry(
                "daily_0",
                "daily-recap",
                serde_json::json!({ "day_start": 1, "total_min": 125, "session_count": 2 }),
            ),
            "clip" => clip_entry("clip-1"),
            "games" => feed_entry(
                "recent_games",
                "recent-games",
                serde_json::json!({
                    "session_count": 14, "total_min": 1860,
                    "games": [{ "game_id": "valorant", "game_name": "Valorant", "minutes": 600 }]
                }),
            ),
            other => feed_entry(
                other,
                "session",
                serde_json::json!({"game_name": "Valorant", "duration_min": 60, "active_min": 60}),
            ),
        }
    };
    h.emit(Event::FeedLoaded {
        response: feed_of(order.iter().map(|k| entry(k)).collect()),
    });
    h
}

/// Bob's feed drew the recap as a standard cell with "Belts moved" cut off.
/// The recap is the tall card (two rows, one column) or the wide card (two
/// columns), whatever order the server sends the cards in.
#[test]
fn the_weekly_recap_is_tall_or_wide_never_one_cell() {
    let orders: [&[&str]; 8] = [
        &["recap", "h2h", "daily", "s1", "s2"],
        &["h2h", "recap", "daily", "s1", "s2"],
        &["daily", "s1", "h2h", "recap", "s2"],
        &["s1", "s2", "daily", "recap", "h2h"],
        &["h2h", "daily", "recap", "s1", "s2", "clip", "games"],
        &["recap", "h2h", "daily", "s1", "clip", "games"],
        &["h2h", "recap", "s1", "clip", "games"],
        &["daily", "recap", "h2h", "s1", "games"],
    ];
    for order in orders {
        let h = crew_with_recap_h2h_daily_and_sessions(order);
        assert!(
            feed_card_types(&h).iter().any(|t| t == "recap"),
            "{order:?}: the feed has the recap"
        );
        let (cw, ch) = cell_size_of_type(&h, "FeedSlot");
        let (w, hgt) = size_of_type(&h, "RecapSlot");
        assert!(
            w >= cw * 1.6 || hgt >= ch * 1.6,
            "{order:?}: the recap is {w}x{hgt} beside a {cw}x{ch} cell"
        );
        assert!(
            h.controls_labelled("See the week").len() <= 1,
            "{order:?}: one control"
        );
    }
}

// ---- head2head is always two columns wide ----

/// A crew of `members` with one pair for each other member, no belt moved.
fn pairs_feed(members: i32, extra: Vec<FeedEntry>) -> FeedResponse {
    let pairs: Vec<_> = (2..=members)
        .map(|n| h2h_pair(&format!("u{n}"), n % 3, 3 - n % 3))
        .collect();
    // The server lists the hero first.
    let mut entries = extra;
    entries.push(h2h_entry(
        members,
        vec![h2h_title("the_carry", "u2", serde_json::json!(4167.0), "h")],
        pairs,
        None,
    ));
    feed_of(entries)
}

fn open_pairs_crew(h: &mut Harness, members: i32, extra: Vec<FeedEntry>) {
    app_with_crew_of(h, members);
    let names: Vec<(String, String)> = std::iter::once(("user-1".to_string(), "Test User".into()))
        .chain((2..=members).map(|n| (format!("u{n}"), format!("Member {n}"))))
        .collect();
    let roster: Vec<(&str, &str)> = names
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    set_roster(h, &roster);
    h.emit(Event::FeedLoaded {
        response: pairs_feed(members, extra),
    });
}

/// The pair rows of the pairs card as `(x, y)`.
fn pair_row_positions(h: &Harness) -> Vec<(f32, f32)> {
    ElementHandle::find_by_element_type_name(h.app(), "PairRow")
        .map(|e| (e.absolute_position().x, e.absolute_position().y))
        .collect()
}

/// From two members on, head2head is the wide card: two grid columns, with the
/// pair rows in two columns (two rows of pairs side by side at five members),
/// a "+N more" line when rows remain, and the seats and the invite under five.
#[test]
fn head2head_is_two_columns_wide_at_every_crew_size_with_the_pair_rows_in_two_columns() {
    for members in [2, 3, 4, 5, 6, 9] {
        let mut h = Harness::new();
        open_pairs_crew(&mut h, members, vec![]);
        assert_eq!(count_of(&h, "Head2headCardPairs"), 1, "{members} members");
        assert_eq!(count_of(&h, "Head2headCardWide"), 0);
        assert_two_columns_wide(&h, "Head2headCardPairs");

        let pairs = (members - 1) as usize;
        let rows = pair_row_positions(&h);
        assert_eq!(rows.len(), pairs.min(4), "{members} members: {rows:?}");
        let mut xs: Vec<i32> = rows.iter().map(|r| r.0 as i32).collect();
        xs.sort();
        xs.dedup();
        if pairs >= 2 {
            assert_eq!(xs.len(), 2, "{members} members: two columns, got {rows:?}");
        }
        // Five members: two rows of pairs side by side.
        if members == 5 {
            let mut ys: Vec<i32> = rows.iter().map(|r| r.1 as i32).collect();
            ys.sort();
            ys.dedup();
            assert_eq!(ys.len(), 2, "two rows of pairs: {rows:?}");
        }
        // The line that stands for the pairs that do not fit.
        let more = pairs.saturating_sub(4);
        assert_eq!(
            h.controls_labelled(&format!("+{more} more")).len(),
            usize::from(more > 0),
            "{members} members"
        );
        // Seats and the red invite stay under five members.
        let seats = (5 - members).max(0) as usize;
        assert_eq!(count_of(&h, "OpenSeat"), seats.min(4), "{members} members");
        assert_eq!(
            h.controls_labelled("Invite your crew").len(),
            usize::from(members < 5),
            "{members} members"
        );
    }
}

/// Behind a hero, head2head takes the first wide slot after it: row 2, two
/// columns, never the one-column side slot.
#[test]
fn head2head_takes_a_wide_slot_behind_a_hero() {
    let mut hero = feed_entry(
        "evt-stream-1",
        "session-preview",
        serde_json::json!({
            "streamer_name": "Jonas", "title": "Ranked", "duration_min": 30,
            "snapshot_urls": ["https://example.invalid/a.jpg"]
        }),
    );
    hero.role = "hero".into();
    for members in [3, 5] {
        let mut h = Harness::new();
        open_pairs_crew(&mut h, members, vec![hero.clone()]);
        let types = feed_card_types(&h);
        assert_eq!(types[0], "session-preview", "{types:?}");
        assert_eq!(count_of(&h, "Head2headCardPairs"), 1);
        assert_two_columns_wide(&h, "Head2headCardPairs");
    }
}

/// A belt that moved for the viewer keeps the versus hero.
#[test]
fn the_versus_hero_stays_the_hero_when_a_belt_moved_for_the_viewer() {
    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    let now = crate::head2head::now_ms();
    let moved = serde_json::json!({
        "title_id": "the_carry", "holder_user_id": "u2", "value": 4201.0, "unit": "h",
        "since_ms": now - 1000, "moved_at_ms": now - 1000, "previous_holder_user_id": "user-1",
    });
    h.emit(Event::FeedLoaded {
        response: feed_of(vec![h2h_entry(
            2,
            vec![moved.clone()],
            vec![h2h_pair("u2", 0, 1)],
            Some(moved),
        )]),
    });
    assert_eq!(count_of(&h, "VersusHero"), 1);
    assert_eq!(count_of(&h, "Head2headCardPairs"), 0);
}

// ---- Crew history and the m3llo+ upsell ----------------------------------

/// A feed with the given `this_week` entries and `memory` entries.
fn feed_with_memory(this_week: Vec<FeedEntry>, memory: Vec<FeedEntry>) -> FeedResponse {
    FeedResponse {
        crew_id: "crew-0".into(),
        sections: vec![
            FeedSection {
                id: "this_week".into(),
                entries: this_week,
            },
            FeedSection {
                id: "memory".into(),
                entries: memory,
            },
        ],
        joined: vec![],
    }
}

/// The m3llo+ upsell as the server sends it, pinned at the end of memory.
fn locked_entry() -> FeedEntry {
    let mut e = feed_entry("locked", "locked", serde_json::json!({}));
    e.role = "locked".into();
    e
}

fn crew_history_shows(h: &Harness) -> bool {
    drawn_texts(h).iter().any(|t| t == "Crew history")
}

fn upsell_shows(h: &Harness) -> bool {
    count_of(h, "LockedCard") > 0
        || drawn_texts(h)
            .iter()
            .any(|t| t == "Unlock your crew's full story")
}

/// Day 0: the server still sends the upsell, and the feed shows neither the
/// "Crew history" heading nor the m3llo+ card. A crew with no history has none
/// of it, so m3llo+ is not offered on an empty feed.
#[test]
fn a_new_crew_shows_no_crew_history_and_no_m3llo_plus_card() {
    for plus in [false, true] {
        let mut h = Harness::new();
        h.settings().borrow_mut().plus_upsell_enabled = plus;
        app_with_crew_of(&mut h, 1);
        set_roster(&h, &[("user-1", "Test User")]);
        h.emit(Event::FeedLoaded {
            response: feed_with_memory(
                vec![h2h_entry(1, vec![], vec![], None)],
                vec![locked_entry()],
            ),
        });
        assert!(
            h.app().get_memory_cards().row_count() == 0,
            "the section is empty (flag {plus})"
        );
        assert!(!crew_history_shows(&h), "no heading (flag {plus})");
        assert!(!upsell_shows(&h), "no upsell (flag {plus})");
    }
}

/// With history the heading shows. The upsell stays off until the client flag
/// `plus_upsell_enabled` is on.
#[test]
fn a_crew_with_history_shows_crew_history_and_the_upsell_only_behind_the_flag() {
    let memory = || vec![clip_entry("old-1"), clip_entry("old-2"), locked_entry()];
    let this_week = || vec![h2h_entry(2, vec![], vec![h2h_pair("u2", 1, 1)], None)];

    let mut h = Harness::new();
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_with_memory(this_week(), memory()),
    });
    assert!(crew_history_shows(&h), "{:?}", drawn_texts(&h));
    assert_eq!(h.app().get_memory_cards().row_count(), 2, "two clips");
    assert!(!upsell_shows(&h), "the flag is off by default");

    let mut h = Harness::new();
    h.settings().borrow_mut().plus_upsell_enabled = true;
    app_with_crew_of(&mut h, 2);
    set_roster(&h, &[("user-1", "Test User"), ("u2", "Jonas")]);
    h.emit(Event::FeedLoaded {
        response: feed_with_memory(this_week(), memory()),
    });
    assert!(crew_history_shows(&h));
    assert!(upsell_shows(&h), "the flag shows the card at the end");
    let last = h
        .app()
        .get_memory_cards()
        .row_data(2)
        .expect("three memory cards");
    assert_eq!(last.card_type.as_str(), "locked", "pinned at the end");
}

/// The flag defaults to off.
#[test]
fn the_plus_upsell_flag_defaults_to_off() {
    assert!(!crate::settings::Settings::default().plus_upsell_enabled);
}
