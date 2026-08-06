//! End-to-end tests over whole user journeys, driven through the headless
//! [`crate::testkit::Harness`].
//!
//! These exercise the real `callbacks::wire_all` / `handlers::handle_event`
//! wiring, so they fail when a change alters what the UI asks core to do or how
//! it reacts to core events.

use i_slint_backend_testing::ElementHandle;
use mello_core::{Command, Event};

use crate::testkit::Harness;

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
fn all_states() -> impl Iterator<Item = (i32, bool, bool)> {
    (0..=5).flat_map(|step| {
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
        // Onboarding finished (step > 3) but not logged in. Reached if the
        // persisted step survives while the session does not.
        (4, false, false),
        (5, false, false),
        // Mid-onboarding, logged in, sign-in requested: onboarding is
        // suppressed by show-sign-in, sign-in by logged-in, and the app by the
        // step range.
        (1, true, true),
        (2, true, true),
        (3, true, true),
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
/// needs a separate later event (`EmailLinked` / `SocialLinked` / `LoggedIn`)
/// or one of the local skip shortcuts. Pinned because it is surprising: an
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
        reason: "invalid credentials".into(),
    });

    assert_eq!(
        h.app().get_login_error().as_str(),
        "invalid credentials",
        "a real failure must be shown to the user"
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

    h.emit(Event::VoiceStateChanged { in_call: true });
    assert!(h.app().get_in_voice());

    h.emit(Event::VoiceStateChanged { in_call: false });
    assert!(!h.app().get_in_voice());
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
