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
            ctx.app.set_onboarding_step(3);
            let mut s = ctx.settings.borrow_mut();
            s.pending_crew_id = None;
            s.pending_crew_name = None;
            s.onboarding_step = 3;
            s.save();
            drop(s);
            let _ = ctx.cmd_tx.try_send(Command::LoadMyCrews);
            dispatch_pending_deep_link(ctx);
        }
        Event::OnboardingFailed { reason } => {
            log::error!("[onboarding] finalization failed: {}", reason);
            ctx.app.set_link_error(reason.into());
        }
        Event::EmailLinked => {
            log::info!("[auth] email linked — onboarding complete");
            ctx.app.set_onboarding_step(4);
            ctx.app.set_logged_in(true);
            let mut s = ctx.settings.borrow_mut();
            s.onboarding_step = 4;
            s.save();
        }
        Event::EmailLinkFailed { reason } => {
            log::warn!("[auth] email-link-failed  reason={}", reason);
            ctx.app.set_link_error(reason.into());
        }
        Event::SocialLinked => {
            log::info!("[auth] social identity linked — onboarding complete");
            ctx.app.set_onboarding_step(4);
            ctx.app.set_logged_in(true);
            let mut s = ctx.settings.borrow_mut();
            s.onboarding_step = 4;
            s.save();
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
            let mut s = ctx.settings.borrow_mut();
            if s.onboarding_step < 4 {
                ctx.app.set_onboarding_step(4);
                s.onboarding_step = 4;
                s.save();
            }
            let _ = ctx
                .cmd_tx
                .try_send(Command::FetchUserAvatar { user_id: uid });

            dispatch_pending_deep_link(ctx);
        }
        Event::LoginFailed { reason } => {
            log::warn!("[auth] login-failed  reason={}", reason);
            ctx.app.set_login_loading(false);
            ctx.app.set_logged_in(false);
            ctx.app.set_login_error(reason.clone().into());

            if reason.is_empty() {
                log::info!("[auth] restore failed — falling back to device auth");
                ctx.app.set_onboarding_step(1);
                let mut s = ctx.settings.borrow_mut();
                s.onboarding_step = 1;
                s.save();
                if let Some(ref device_id) = s.device_id {
                    let _ = ctx.cmd_tx.try_send(Command::DeviceAuth {
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
                let _ = ctx.cmd_tx.try_send(Command::ResolveCrewInvite { code });
            }
            DeepLink::Crew { id } => {
                log::info!("[deep_link] dispatching pending crew select id={}", id);
                let _ = ctx.cmd_tx.try_send(Command::SelectCrew { crew_id: id });
            }
        }
    }
}
