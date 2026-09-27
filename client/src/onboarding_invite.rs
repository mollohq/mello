//! An invite link on a fresh install (#68).
//!
//! A fresh install opened from `mello://join/<code>` skips step 1. The client
//! resolves the code before an account exists (core uses the `http_key`), and
//! opens step 2 with the invited crew shown. Finalize then joins that crew by
//! its invite code, so a private crew works too.
//!
//! An invite that cannot be used lands on step 1 with a message. Step 1 always
//! offers a crew or "create your own", so the user is never stuck.
//!
//! A user with a device account, or a logged-in user, keeps the join modal:
//! the pending link is sent after sign-in (`handlers::auth`).

use std::cell::RefCell;
use std::rc::Rc;

use mello_core::crew::{InviteError, ResolvedInvite};
use mello_core::Command;

use crate::app_context::AppContext;
use crate::deep_link::DeepLink;
use crate::handlers::{invite_join_error_message, InviteSource};
use crate::onboarding::{Input, OnboardingState};

/// Does an invite link go into onboarding, instead of the join modal?
///
/// Only for a fresh install that has not created its account: no session, no
/// device account, and a step before the account exists.
pub fn opens_onboarding(app: &crate::MainWindow, settings: &crate::Settings) -> bool {
    opens_onboarding_at(
        OnboardingState::from_step(app.get_onboarding_step()),
        app.get_logged_in(),
        settings.has_device_account(),
    )
}

fn opens_onboarding_at(state: OnboardingState, logged_in: bool, has_device_account: bool) -> bool {
    let before_account = matches!(
        state,
        OnboardingState::Loading | OnboardingState::PickCrew | OnboardingState::PickAvatar
    );
    !logged_in && !has_device_account && before_account
}

/// At startup, resolve an invite link now when it goes into onboarding.
///
/// `state` is the step that startup resumes. Call before `onboarding::resume`:
/// core runs commands in order, so the resolve answers before crew discovery,
/// and step 1 does not show first. Any other link stays pending until the
/// user signs in.
pub fn dispatch_at_startup(ctx: &AppContext, state: OnboardingState) {
    let has_device_account = ctx.settings.borrow().has_device_account();
    if !opens_onboarding_at(state, false, has_device_account) {
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

/// Show an invite that a previous run stored, for a restart on step 2.
fn show_pending(ctx: &AppContext) {
    let name = ctx.settings.borrow().pending_invite_crew_name.clone();
    if let Some(name) = name.filter(|_| ctx.settings.borrow().pending_invite_code.is_some()) {
        set_crew(&ctx.app, &name);
    }
}

/// The invite resolved: store it and open step 2 with the crew shown.
pub fn accept(ctx: &AppContext, code: String, invite: ResolvedInvite) {
    log::info!(
        "[invite] onboarding joins crew={:?} id={} — opening step 2",
        invite.crew_name,
        invite.crew_id
    );
    {
        let mut s = ctx.settings.borrow_mut();
        s.pending_invite_code = Some(code);
        s.pending_invite_crew_name = Some(invite.crew_name.clone());
        // One crew only: the invite replaces a crew picked before.
        s.pending_crew_id = None;
        s.pending_crew_name = None;
        s.save();
    }
    set_crew(&ctx.app, &invite.crew_name);
    ctx.app.set_onboarding_invite_error("".into());
    crate::onboarding::advance(ctx, Input::InviteResolved);
}

/// The invite did not resolve: step 1 says why. Discovery moves `Loading` on
/// to step 1, so this does not change the step.
pub fn resolve_failed(ctx: &AppContext, error: InviteError) {
    log::warn!("[invite] onboarding invite did not resolve: {error:?} — step 1 with a message");
    ctx.app
        .set_onboarding_invite_error(crate::handlers::invite_resolve_error_message(error).into());
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
/// Does not save the settings. Every caller moves onboarding next, and the
/// single writer in `onboarding` saves them.
pub fn clear(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>) {
    {
        let mut s = settings.borrow_mut();
        s.pending_invite_code = None;
        s.pending_invite_crew_name = None;
    }
    set_crew(app, "");
}

fn set_crew(app: &crate::MainWindow, name: &str) {
    app.set_onboarding_invite_crew_name(name.into());
    app.set_onboarding_invite_crew_initials(crate::converters::make_initials(name).into());
}
