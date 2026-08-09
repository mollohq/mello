use mello_core::{Command, Event};

use crate::app_context::AppContext;
use crate::converters::make_initials;
use crate::deep_link::DeepLink;

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
            ctx.app.set_user_id(user.id.into());
            ctx.app
                .set_user_initials(make_initials(&user.display_name).into());
            ctx.app.set_user_name(user.display_name.into());
            ctx.app.set_user_tag(user.tag.into());
            ctx.app.set_is_returning_user(!created);
        }
        Event::OnboardingReady { user } => {
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
            // The account exists, but identity linking is still offered — so
            // this is LinkIdentity, not Done.
            crate::onboarding::advance(ctx, crate::onboarding::Input::AccountReady);
            let _ = ctx.cmd_tx.send(Command::LoadMyCrews);
            dispatch_pending_deep_link(ctx);
        }
        Event::OnboardingFailed { reason } => {
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
            ctx.app.set_login_error(reason.clone().into());

            if reason.is_empty() {
                log::info!("[auth] restore failed — falling back to device auth");
                crate::onboarding::advance(ctx, crate::onboarding::Input::RestoreFailed);
                let s = ctx.settings.borrow();
                if let Some(ref device_id) = s.device_id {
                    let _ = ctx.cmd_tx.send(Command::DeviceAuth {
                        device_id: device_id.clone(),
                    });
                }
            }
        }
        _ => {}
    }
}

fn dispatch_pending_deep_link(ctx: &AppContext) {
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
