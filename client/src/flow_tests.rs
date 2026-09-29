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
use mello_core::{decode_clip_waveform, Command, Event};
use slint::Model;

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
    });
    finalize(&h);
    let second = h.commands();

    assert_eq!(finalize_device_ids(&first), finalize_device_ids(&second));
    assert_eq!(
        finalize_invite(&second),
        Some((Some("NITE-0001".into()), None, None))
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
    assert!(h.app().get_has_device_account());
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
const RETURNING_SIGN_IN: &str = "Sign in";

fn with_device_account(h: &Harness) {
    h.settings().borrow_mut().device_id = Some("dev-abc".into());
}

/// The sign-in controls on screen, by label.
fn sign_in_controls(h: &Harness) -> Vec<&'static str> {
    [HAVE_ACCOUNT, RETURNING_SIGN_IN]
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

    assert!(!h.app().get_has_device_account());
    assert_eq!(sign_in_controls(&h), vec![HAVE_ACCOUNT]);
}

/// ★ Regression (#67, R1): a machine with a device account shows no
/// "I already have an account" on step 1.
#[test]
fn a_device_account_hides_i_already_have_an_account() {
    let h = Harness::new();
    with_device_account(&h);

    resume(h.ctx(), OnboardingState::PickCrew);

    assert!(h.app().get_has_device_account());
    assert!(
        h.controls_labelled(HAVE_ACCOUNT).is_empty(),
        "this machine has a device account"
    );
}

/// ★ Regression (#70, R5): a returning user who logged out sees exactly one
/// sign-in control, the returning-user one.
#[test]
fn a_returning_user_sees_exactly_one_sign_in_control() {
    let mut h = Harness::new();
    with_device_account(&h);
    h.emit(Event::LoggedIn {
        user: sample_user(),
    });

    h.app().invoke_logout();
    let cmds = h.commands();
    assert!(
        cmds.iter().any(|c| matches!(c, Command::DeviceAuth { .. })),
        "logout authenticates the device account for the returning-user control, got {cmds:?}"
    );
    h.emit(Event::DeviceAuthed {
        user: sample_user(),
        created: false,
    });

    assert_eq!(h.app().get_onboarding_step(), 1);
    assert_eq!(sign_in_controls(&h), vec![RETURNING_SIGN_IN]);
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

fn game_rollup_feed() -> mello_core::crew_events::FeedResponse {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    mello_core::crew_events::FeedResponse {
        crew_id: "crew-0".into(),
        sections: vec![mello_core::crew_events::FeedSection {
            id: "this_week".into(),
            entries: vec![mello_core::crew_events::FeedEntry {
                id: "game_rollup".into(),
                entry_type: "rollup".into(),
                role: "standard".into(),
                size: "md".into(),
                ts,
                data: serde_json::json!({
                    "session_count": 5,
                    "total_min": 720,
                    "lines": [
                        {
                            "player_name": "ostkatt",
                            "game_name": "Valorant",
                            "total_min": 300,
                            "sessions": 2
                        },
                        {
                            "player_name": "bob",
                            "game_name": "Minecraft",
                            "total_min": 240,
                            "sessions": 1
                        },
                        {
                            "player_name": "kim",
                            "game_name": "Counter-Strike 2",
                            "total_min": 180,
                            "sessions": 1
                        }
                    ]
                }),
            }],
        }],
    }
}

/// Pruned routine play renders as a crew play rollup card with per-actor lines.
#[test]
fn game_rollup_renders_card_with_lines() {
    let mut h = Harness::new();
    logged_in_app(&mut h);

    h.emit(Event::FeedLoaded {
        response: game_rollup_feed(),
    });

    let cards: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "GameRollupCard").collect();
    assert!(
        !cards.is_empty(),
        "expected a GameRollupCard in the feed for a rollup entry"
    );

    let lines: Vec<_> =
        ElementHandle::find_by_element_type_name(h.app(), "GameRollupLineRow").collect();
    assert_eq!(
        lines.len(),
        3,
        "rollup card should render one line row per actor"
    );
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
