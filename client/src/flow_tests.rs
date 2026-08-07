//! End-to-end tests over whole user journeys, driven through the headless
//! [`crate::testkit::Harness`].
//!
//! These exercise the real `callbacks::wire_all` / `handlers::handle_event`
//! wiring, so they fail when a change alters what the UI asks core to do or how
//! it reacts to core events.

use i_slint_backend_testing::ElementHandle;
use mello_core::{Command, Event};

use crate::testkit::{Harness, MainWindow};

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

/// Drive the step-3 "continue" that fires `FinalizeOnboarding`.
///
/// The nickname is a Slint `out` property so Rust cannot set it; it is not
/// what these tests are about.
fn finalize(h: &Harness) {
    h.app().invoke_onboarding_continue(3);
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
        Command::SendMessage { content, reply_to } => Some((content.clone(), reply_to.clone())),
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
        Command::SendMessage { content, reply_to } => Some((content.clone(), reply_to.clone())),
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
            Command::EditMessage { message_id, new_body }
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
    h.app().invoke_discover_join_invite("ABC123".into());

    let cmds = h.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::JoinCrew { crew_id } if crew_id == "crew-7")),
        "joining a listed crew must send JoinCrew, got {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::JoinByInviteCode { code } if code == "ABC123")),
        "an invite code must go through JoinByInviteCode, got {cmds:?}"
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

/// Social sign-in dismisses the panel, so the user is not left looking at a
/// sign-in form while the provider flow runs.
#[test]
fn social_signin_dismisses_the_sign_in_panel() {
    let h = Harness::new();
    h.app().set_show_sign_in(true);

    h.app().invoke_signin_google();

    assert!(!h.app().get_show_sign_in());
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
