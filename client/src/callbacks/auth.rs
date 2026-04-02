use slint::ComponentHandle;

use crate::app_context::AppContext;
use mello_core::Command;

pub fn wire(ctx: &AppContext) {
    // --- Login ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_login(move |email, password| {
            if let Some(app) = app_weak.upgrade() {
                app.set_login_loading(true);
                app.set_login_error("".into());
            }
            let _ = cmd.try_send(Command::Login {
                email: email.to_string(),
                password: password.to_string(),
            });
        });
    }

    // --- Logout ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let settings_ref = ctx.settings.clone();
        let avatar_cache_ref = ctx.avatar_cache.clone();
        ctx.app.on_logout(move || {
            avatar_cache_ref.borrow_mut().clear();
            let _ = cmd.try_send(Command::Logout);
            if let Some(app) = app_weak.upgrade() {
                app.set_logged_in(false);
                app.set_user_name("".into());
                app.set_user_initials("".into());
                app.set_user_tag("".into());
                app.set_user_avatar(slint::Image::default());
                app.set_has_user_avatar(false);
                app.set_active_crew_id("".into());
                app.set_onboarding_step(1);
            }
            let mut s = settings_ref.borrow_mut();
            s.onboarding_step = 1;
            s.save();
            log::info!("Logged out — returning to onboarding step 1");
            if let Some(ref device_id) = s.device_id {
                let _ = cmd.try_send(Command::DeviceAuth {
                    device_id: device_id.clone(),
                });
            }
        });
    }

    // --- Sign-in panel: social auth (returning user) ---
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_steam(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_show_sign_in(false);
            }
            let _ = cmd.try_send(Command::AuthSteam);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_google(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_show_sign_in(false);
            }
            let _ = cmd.try_send(Command::AuthGoogle);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_twitch(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_show_sign_in(false);
            }
            let _ = cmd.try_send(Command::AuthTwitch);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_discord(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_show_sign_in(false);
            }
            let _ = cmd.try_send(Command::AuthDiscord);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_apple(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_show_sign_in(false);
            }
            let _ = cmd.try_send(Command::AuthApple);
        });
    }
}
