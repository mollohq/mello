use mello_core::{Command, Event};

use crate::app_context::AppContext;
use crate::converters::make_initials;
use crate::deep_link::DeepLink;
use crate::onboarding::OnboardingState;

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::Restoring => {
            log::info!("[auth] restoring session…");
            ctx.app.set_login_loading(true);
        }
        Event::DeviceAuthed { user, created } => {
            log::info!(
                "[auth] device-authed  user_id={} name={} tag={} created={}",
                user.id,
                user.display_name,
                user.tag,
                created
            );
            let uid = user.id.clone();
            ctx.app.set_user_id(user.id.into());
            ctx.app
                .set_user_initials(make_initials(&user.display_name).into());
            ctx.app.set_user_name(user.display_name.into());
            ctx.app.set_user_tag(user.tag.into());
            ctx.app.set_is_returning_user(!created);

            // Device auth after a failed restore (#71). The user finished
            // onboarding on this machine, so an existing device account opens
            // the app, the same as a restored session.
            if ctx.session_recovery.replace(false) {
                if created {
                    log::info!(
                        "[auth] restore failed and the device account is new — back to step 1"
                    );
                    crate::onboarding::advance(ctx, crate::onboarding::Input::RestoreFailed);
                } else {
                    log::info!("[auth] session lost, device account found — opening the app");
                    ctx.app.set_logged_in(true);
                    crate::onboarding::advance(ctx, crate::onboarding::Input::SessionRestored);
                    let _ = ctx.cmd_tx.send(Command::LoadMyCrews);
                    let _ = ctx.cmd_tx.send(Command::FetchUserAvatar { user_id: uid });
                    dispatch_pending_deep_link(ctx);
                }
            }
        }
        Event::OnboardingReady { user } => {
            ctx.app.set_onboarding_busy(false);
            log::info!(
                "[onboarding] ready — user_id={} name={}",
                user.id,
                user.display_name
            );
            ctx.app.set_user_id(user.id.into());
            ctx.app
                .set_user_initials(make_initials(&user.display_name).into());
            ctx.app.set_user_name(user.display_name.into());
            ctx.app.set_user_tag(user.tag.into());
            ctx.app.set_logged_in(true);
            // Release the pending crew avatar only now that onboarding has
            // actually succeeded. It is deliberately retained through a failed
            // attempt so a retry still carries it.
            *ctx.new_crew_avatar_b64.lock().unwrap() = None;
            {
                let mut s = ctx.settings.borrow_mut();
                s.pending_crew_id = None;
                s.pending_crew_name = None;
            }
            // Finalize joined the invited crew, if any. Forget the invite
            // before the pending link below, so it does not open again.
            crate::onboarding_invite::clear(&ctx.app, &ctx.settings);
            // The account exists, but identity linking is still offered — so
            // this is LinkIdentity, not Done.
            crate::onboarding::advance(ctx, crate::onboarding::Input::AccountReady);
            let _ = ctx.cmd_tx.send(Command::LoadMyCrews);
            dispatch_pending_deep_link(ctx);
        }
        Event::OnboardingInviteFailed { error } => {
            crate::onboarding_invite::join_failed(ctx, error);
        }
        Event::OnboardingFailed { reason } => {
            // Release the guard: the user is still on the same step and must be
            // able to retry.
            ctx.app.set_onboarding_busy(false);
            log::error!("[onboarding] finalization failed: {}", reason);
            ctx.app.set_link_error(reason.into());
        }
        Event::EmailLinked => {
            log::info!("[auth] email linked — onboarding complete");
            ctx.app.set_logged_in(true);
            crate::onboarding::advance(ctx, crate::onboarding::Input::IdentitySettled);
        }
        Event::EmailLinkFailed { reason } => {
            log::warn!("[auth] email-link-failed  reason={}", reason);
            ctx.app.set_link_error(reason.into());
        }
        Event::SocialLinked => {
            log::info!("[auth] social identity linked — onboarding complete");
            ctx.app.set_logged_in(true);
            crate::onboarding::advance(ctx, crate::onboarding::Input::IdentitySettled);
        }
        // No UI affordance deletes an account yet — `Command::DeleteAccount`
        // exists for the release smoke test, which drives it from a scenario
        // and asserts on the event rather than on the window. Log only; give
        // these arms real UI behaviour when a delete-account setting lands.
        Event::AccountDeleted => {
            log::info!("[auth] account deleted — session cleared");
        }
        Event::AccountDeleteFailed { reason } => {
            log::warn!("[auth] account-delete-failed  reason={}", reason);
        }
        Event::SocialLinkFailed { reason } => {
            log::warn!("[auth] social-link-failed  reason={}", reason);
            ctx.app.set_login_loading(false);
            ctx.app.set_link_error(reason.into());
        }
        Event::LoggedIn { user } => {
            log::info!(
                "[auth] logged-in  user_id={} name={} tag={}",
                user.id,
                user.display_name,
                user.tag
            );
            ctx.app.set_logged_in(true);
            ctx.app.set_login_loading(false);
            ctx.app.set_show_sign_in(false);
            ctx.app.set_login_error("".into());
            ctx.app.set_login_account_missing(false);
            let uid = user.id.clone();
            ctx.app.set_user_id(user.id.into());
            ctx.app
                .set_user_initials(make_initials(&user.display_name).into());
            ctx.app.set_user_name(user.display_name.into());
            ctx.app.set_user_tag(user.tag.into());
            crate::onboarding::advance(ctx, crate::onboarding::Input::SessionRestored);
            let _ = ctx.cmd_tx.send(Command::FetchUserAvatar { user_id: uid });

            dispatch_pending_deep_link(ctx);
        }
        Event::LoginFailed { reason } => {
            log::warn!("[auth] login-failed  reason={}", reason);
            ctx.app.set_login_loading(false);
            ctx.app.set_logged_in(false);

            if ctx.session_recovery.replace(false) {
                // Device auth after a failed restore failed too. Nobody was
                // signing in, so there is no error to show: go to step 1.
                log::warn!("[auth] device auth after a failed restore failed — back to step 1");
                crate::onboarding::advance(ctx, crate::onboarding::Input::RestoreFailed);
                return;
            }

            if reason.is_empty() {
                restore_failed(ctx);
                return;
            }

            let failure = SignInFailure::from_reason(&reason);
            ctx.app.set_login_error(failure.message.into());
            ctx.app.set_login_account_missing(failure.account_missing);
        }
        _ => {}
    }
}

/// The saved session did not restore (an empty `LoginFailed` reason).
///
/// A user who finished onboarding on this machine and has a device account
/// gets device auth first, and the window stays on the restore wait. The
/// answer opens the app or goes to step 1 (see `Event::DeviceAuthed`). Before
/// #71 this went to step 1 at once, and the device account that owns the
/// user's crews waited behind a "Sign in" control.
fn restore_failed(ctx: &AppContext) {
    let device_id = {
        let s = ctx.settings.borrow();
        s.device_id.clone().filter(|_| s.has_device_account())
    };
    let onboarded =
        OnboardingState::from_step(ctx.app.get_onboarding_step()) == OnboardingState::Done;

    match device_id {
        Some(device_id) if onboarded => {
            log::info!("[auth] restore failed — device auth decides between the app and step 1");
            ctx.session_recovery.set(true);
            let _ = ctx.cmd_tx.send(Command::DeviceAuth { device_id });
        }
        Some(device_id) => {
            log::info!("[auth] restore failed — falling back to device auth");
            crate::onboarding::advance(ctx, crate::onboarding::Input::RestoreFailed);
            let _ = ctx.cmd_tx.send(Command::DeviceAuth { device_id });
        }
        None => {
            log::info!("[auth] restore failed — no device account, back to step 1");
            crate::onboarding::advance(ctx, crate::onboarding::Input::RestoreFailed);
        }
    }
}

/// What the sign-in panel shows when a sign-in fails.
///
/// The core reports the server's text, for example "Authentication failed:
/// User account not found.". A user cannot act on that (#67), so the known
/// server answers become plain messages.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SignInFailure {
    pub message: String,
    /// No account has this identity. The panel offers "Start as a new player".
    pub account_missing: bool,
}

impl SignInFailure {
    pub(crate) fn from_reason(reason: &str) -> Self {
        let lower = reason.to_ascii_lowercase();
        let (message, account_missing) = if lower.contains("account not found") {
            ("No account found.", true)
        } else if lower.contains("invalid credentials") {
            ("Wrong email or password.", false)
        } else if lower.starts_with("authentication failed") {
            ("Sign-in failed. Try again.", false)
        } else {
            // Messages the client composes itself, for example
            // "Discord sign-in failed: …", are already written for the user.
            return Self {
                message: reason.to_string(),
                account_missing: false,
            };
        };
        Self {
            message: message.to_string(),
            account_missing,
        }
    }
}

fn dispatch_pending_deep_link(ctx: &AppContext) {
    // An invite that onboarding stored, when the user signed in to an
    // existing account instead of finishing onboarding (#68). The account
    // exists now, so the invite opens the join modal.
    let stored_invite = ctx.settings.borrow().pending_invite_code.clone();
    if let Some(code) = stored_invite {
        crate::onboarding_invite::clear(&ctx.app, &ctx.settings);
        ctx.settings.borrow().save();
        log::info!(
            "[deep_link] dispatching the invite stored by onboarding code={}",
            code
        );
        let _ = ctx.cmd_tx.send(Command::ResolveCrewInvite { code });
    }

    let link = ctx.pending_deep_link.borrow_mut().take();
    if let Some(deep_link) = link {
        match deep_link {
            DeepLink::Join { code } => {
                log::info!("[deep_link] dispatching pending join code={}", code);
                let _ = ctx.cmd_tx.send(Command::ResolveCrewInvite { code });
            }
            DeepLink::Crew { id } => {
                log::info!("[deep_link] dispatching pending crew select id={}", id);
                let _ = ctx.cmd_tx.send(Command::SelectCrew { crew_id: id });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SignInFailure;

    /// #67: the raw server text for an unknown account never reaches the user.
    #[test]
    fn an_unknown_account_reads_as_a_plain_message_with_a_way_forward() {
        let f = SignInFailure::from_reason("Authentication failed: User account not found.");
        assert_eq!(f.message, "No account found.");
        assert!(f.account_missing);
    }

    #[test]
    fn other_server_answers_read_as_plain_messages() {
        let f = SignInFailure::from_reason("Authentication failed: Invalid credentials.");
        assert_eq!(f.message, "Wrong email or password.");
        assert!(!f.account_missing);

        let f = SignInFailure::from_reason("Authentication failed: something new");
        assert_eq!(f.message, "Sign-in failed. Try again.");
        assert!(!f.account_missing);
    }

    #[test]
    fn client_messages_pass_through() {
        let reason = "Discord sign-in failed: Timeout waiting for authentication";
        let f = SignInFailure::from_reason(reason);
        assert_eq!(f.message, reason);
        assert!(!f.account_missing);
    }
}
