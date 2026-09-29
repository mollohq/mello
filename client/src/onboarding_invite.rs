//! An invite link on a fresh install (#68).
//!
//! A fresh install opened from `mello://join/<code>` skips step 1. The client
//! resolves the code before an account exists (core uses the `http_key`), and
//! opens the welcome screen: who invited the user, to which crew. "Join" opens
//! step 2 with the invited crew shown. Finalize then joins that crew by its
//! invite code, so a private crew works too. "Not now" opens step 1 and
//! forgets the invite.
//!
//! An invite that cannot be used lands on step 1 with a message. Step 1 always
//! offers a crew or "create your own", so the user is never stuck.
//!
//! A user with a device account, or a logged-in user, keeps the join modal:
//! the pending link is sent after sign-in (`handlers::auth`).
//!
//! The web lounge cannot always hand the invite to the app. Step 1 has the
//! invite-code card for that: the user pastes the link or the code, and the
//! resolve takes the same path as a deep link (CREW-INVITES §7.1). The card
//! also works for a user with a device account who logged out: that user is
//! on step 1, and finalize joins the crew into the existing account.

use std::cell::RefCell;
use std::rc::Rc;

use mello_core::crew::{InviteError, ResolvedInvite};
use mello_core::Command;
use tokio::sync::mpsc::UnboundedSender;

use crate::app_context::AppContext;
use crate::deep_link::DeepLink;
use crate::handlers::{invite_join_error_message, invite_resolve_error_message, InviteSource};
use crate::onboarding::{EffectCtx, Input, OnboardingState};

/// Does an invite go into onboarding, instead of the join modal?
///
/// For a fresh install that has not created its account: no session, no
/// device account, and a step before the account exists.
///
/// Also for an invite that the user typed into the card on step 1, with a
/// device account: the user logged out, has no session, and cannot join in
/// the modal. Finalize joins the crew into the device account instead.
pub fn opens_onboarding(app: &crate::MainWindow, settings: &crate::Settings) -> bool {
    opens_onboarding_at(
        OnboardingState::from_step(app.get_onboarding_step()),
        app.get_logged_in(),
        settings.has_device_account(),
        app.get_onboarding_invite_code_checking(),
    )
}

fn opens_onboarding_at(
    state: OnboardingState,
    logged_in: bool,
    has_device_account: bool,
    typed_in_card: bool,
) -> bool {
    let before_account = matches!(
        state,
        OnboardingState::Loading
            | OnboardingState::PickCrew
            | OnboardingState::InviteWelcome
            | OnboardingState::PickAvatar
    );
    !logged_in && (!has_device_account || typed_in_card) && before_account
}

/// At startup, resolve an invite link now when it goes into onboarding.
///
/// `state` is the step that startup resumes. Call before `onboarding::resume`:
/// core runs commands in order, so the resolve answers before crew discovery,
/// and step 1 does not show first. Any other link stays pending until the
/// user signs in.
pub fn dispatch_at_startup(ctx: &AppContext, state: OnboardingState) {
    let has_device_account = ctx.settings.borrow().has_device_account();
    if !opens_onboarding_at(state, false, has_device_account, false) {
        return;
    }
    show_pending(ctx);

    let is_join = matches!(*ctx.pending_deep_link.borrow(), Some(DeepLink::Join { .. }));
    if !is_join {
        return;
    }
    if let Some(DeepLink::Join { code }) = ctx.pending_deep_link.borrow_mut().take() {
        log::info!("[invite] fresh install opened from an invite — resolving {code} before step 2");
        let _ = ctx.cmd_tx.send(Command::ResolveCrewInvite { code });
    }
}

/// The step that startup resumes, given the persisted one.
///
/// The welcome screen shows the stored invite. With no stored invite it has
/// nothing to show, so startup opens step 1 instead of an empty screen.
pub fn resume_state(settings: &crate::Settings, state: OnboardingState) -> OnboardingState {
    let stored = settings.pending_invite_code.is_some() && settings.pending_invite.is_some();
    if state == OnboardingState::InviteWelcome && !stored {
        log::warn!("[invite] welcome screen persisted with no stored invite — resuming step 1");
        return OnboardingState::PickCrew;
    }
    state
}

/// Show an invite that a previous run stored, for a restart on step 2.
fn show_pending(ctx: &AppContext) {
    let name = ctx.settings.borrow().pending_invite_crew_name.clone();
    if let Some(name) = name.filter(|_| ctx.settings.borrow().pending_invite_code.is_some()) {
        set_crew(&ctx.app, &name);
    }
}

/// The invite resolved: store it and open the welcome screen.
///
/// Stores the whole invite, not only the code: a restart on the welcome
/// screen shows it again from Settings. Resolving again would need the
/// network, and a failure there would leave the screen with nothing to show.
pub fn accept(ctx: &AppContext, code: String, invite: ResolvedInvite) {
    log::info!(
        "[invite] onboarding invite crew={:?} id={} inviter={:?} — opening the welcome screen",
        invite.crew_name,
        invite.crew_id,
        invite.inviter.as_ref().map(|p| p.display_name.as_str()),
    );
    {
        let mut s = ctx.settings.borrow_mut();
        s.pending_invite_code = Some(code);
        s.pending_invite_crew_name = Some(invite.crew_name.clone());
        s.pending_invite = Some(invite);
        s.onboarding_via_invite = true;
        // One crew only: the invite replaces a crew picked before.
        s.pending_crew_id = None;
        s.pending_crew_name = None;
        s.save();
    }
    // Also when the welcome screen is already open: a second link replaces
    // the first, and the entry effect runs only on a change of state.
    show_stored(&ctx.app, &ctx.settings);
    ctx.app.set_onboarding_invite_error("".into());
    // The card that sent this resolve is done. The typed text stays until
    // "Not now" clears it.
    ctx.app.set_onboarding_invite_code_checking(false);
    ctx.app.set_onboarding_invite_code_error("".into());
    crate::onboarding::advance(ctx, Input::InviteResolved);
}

/// "Open invite" on step 1, or Enter in the field: what the user typed.
///
/// A text that is no invite gets the message in the card and no network
/// call. A valid one sends `ResolveCrewInvite`, and the answer takes the
/// path of a deep link (`accept`, `resolve_failed`). A second press while the
/// resolve runs does nothing.
pub fn open_typed(app: &crate::MainWindow, cmd_tx: &UnboundedSender<Command>, text: &str) {
    if app.get_onboarding_invite_code_checking() {
        log::debug!("[invite] card: a resolve is running — ignoring");
        return;
    }
    match crate::deep_link::parse_invite_input(text) {
        Some(code) => {
            log::info!("[invite] card: resolving {code}");
            app.set_onboarding_invite_code_error("".into());
            app.set_onboarding_invite_code_checking(true);
            let _ = cmd_tx.send(Command::ResolveCrewInvite { code });
        }
        None => {
            log::info!("[invite] card: the text is not an invite — no network call");
            app.set_onboarding_invite_code_error(
                invite_resolve_error_message(InviteError::InvalidCode, InviteSource::TypedCode)
                    .into(),
            );
        }
    }
}

/// The user edited the field in the card: the message no longer applies.
pub fn code_edited(app: &crate::MainWindow) {
    app.set_onboarding_invite_code_error("".into());
}

/// "Back" on step 2 of the invite path: the welcome screen, with the invite
/// kept. The welcome screen shows it again from `Settings`.
pub fn back(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>, fx: &EffectCtx) {
    log::info!("[invite] step 2: back — opening the welcome screen, the invite is kept");
    crate::onboarding::advance_with(
        app,
        settings,
        fx,
        Input::GoBackTo(OnboardingState::InviteWelcome),
    );
}

/// "Join" on the welcome screen: step 2, which keeps the invite.
pub fn join(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>, fx: &EffectCtx) {
    log::info!("[invite] welcome screen: join — opening step 2");
    crate::onboarding::advance_with(app, settings, fx, Input::InviteAccepted);
}

/// "Not now" on the welcome screen: step 1, and the invite is forgotten.
/// Finalize then joins or creates the crew that the user picks there.
pub fn decline(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>, fx: &EffectCtx) {
    log::info!("[invite] welcome screen: not now — forgetting the invite, opening step 1");
    clear(app, settings);
    crate::onboarding::advance_with(app, settings, fx, Input::InviteDeclined);
}

/// Put the stored invite on the welcome screen and on step 2.
///
/// The entry effect of the welcome screen, so every way in shows the same
/// data, a restart included.
pub fn show_stored(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>) {
    let invite = settings.borrow().pending_invite.clone();
    match invite {
        Some(invite) => show(app, &invite),
        None => log::warn!("[invite] welcome screen with no stored invite"),
    }
}

/// The invite did not resolve: step 1 says why. Discovery moves `Loading` on
/// to step 1, so this does not change the step.
///
/// An invite that the user typed shows the message in the card, with the
/// field marked. A link that opened the app shows it above the crews.
pub fn resolve_failed(ctx: &AppContext, error: InviteError) {
    if ctx.app.get_onboarding_invite_code_checking() {
        log::warn!("[invite] card: the invite did not resolve: {error:?} — message in the card");
        ctx.app.set_onboarding_invite_code_checking(false);
        ctx.app.set_onboarding_invite_code_error(
            invite_resolve_error_message(error, InviteSource::TypedCode).into(),
        );
        return;
    }
    log::warn!("[invite] onboarding invite did not resolve: {error:?} — step 1 with a message");
    ctx.app.set_onboarding_invite_error(
        invite_resolve_error_message(error, InviteSource::Link).into(),
    );
}

/// Finalize could not join the invited crew. The account exists.
///
/// A failure that a retry can fix stays on step 2 with the message. A code
/// that no longer works, a full crew or a refusal goes back to step 1: the
/// user picks another crew there.
pub fn join_failed(ctx: &AppContext, error: InviteError) {
    ctx.app.set_onboarding_busy(false);
    let message = invite_join_error_message(error, InviteSource::Link);
    if error == InviteError::Failed {
        log::warn!("[invite] finalize could not join the invited crew — retry on step 2");
        ctx.app.set_link_error(message.into());
        return;
    }
    log::warn!("[invite] finalize cannot join the invited crew: {error:?} — back to step 1");
    clear(&ctx.app, &ctx.settings);
    ctx.app.set_onboarding_invite_error(message.into());
    crate::onboarding::advance(ctx, Input::GoBackTo(OnboardingState::PickCrew));
}

/// Forget the invite: the user chose another crew, or onboarding finished.
///
/// Clears what the user typed in the card too. It does not end a resolve in
/// flight: only the answer does.
///
/// Does not save the settings. Every caller moves onboarding next, and the
/// single writer in `onboarding` saves them.
pub fn clear(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>) {
    {
        let mut s = settings.borrow_mut();
        s.pending_invite_code = None;
        s.pending_invite_crew_name = None;
        s.pending_invite = None;
    }
    set_crew(app, "");
    app.set_onboarding_invite_code_text("".into());
    app.set_onboarding_invite_code_error("".into());
    app.set_onboarding_invite_highlight("".into());
    app.set_onboarding_invite_member_count(0);
    app.set_onboarding_invite_members(Rc::new(slint::VecModel::default()).into());
    app.set_onboarding_invite_inviter(Default::default());
}

fn set_crew(app: &crate::MainWindow, name: &str) {
    app.set_onboarding_invite_crew_name(name.into());
    app.set_onboarding_invite_crew_initials(crate::converters::make_initials(name).into());
}

fn show(app: &crate::MainWindow, invite: &ResolvedInvite) {
    set_crew(app, &invite.crew_name);
    app.set_onboarding_invite_highlight(invite.highlight.as_str().into());
    app.set_onboarding_invite_member_count(invite.member_count);
    let members: Vec<_> = invite
        .members
        .iter()
        .map(crate::converters::invite_person)
        .collect();
    app.set_onboarding_invite_members(Rc::new(slint::VecModel::from(members)).into());
    app.set_onboarding_invite_inviter(
        invite
            .inviter
            .as_ref()
            .map(crate::converters::invite_person)
            .unwrap_or_default(),
    );
}
