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
                begin_sign_in(&app);
            }
            let _ = cmd.send(Command::Login {
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
        let fx = crate::onboarding::EffectCtx::from_ctx(ctx);
        let avatar_cache_ref = ctx.avatar_cache.clone();
        ctx.app.on_logout(move || {
            avatar_cache_ref.borrow_mut().clear();
            let _ = cmd.send(Command::Logout);
            if let Some(app) = app_weak.upgrade() {
                app.set_logged_in(false);
                app.set_user_name("".into());
                app.set_user_initials("".into());
                app.set_user_tag("".into());
                app.set_user_avatar(slint::Image::default());
                app.set_has_user_avatar(false);
                crate::converters::set_active_crew(&app, "");
                crate::onboarding::advance_with(
                    &app,
                    &settings_ref,
                    &fx,
                    crate::onboarding::Input::LoggedOut,
                );
            }
            let s = settings_ref.borrow();
            log::info!("Logged out — returning to crew selection");
            if let Some(ref device_id) = s.device_id {
                let _ = cmd.send(Command::DeviceAuth {
                    device_id: device_id.clone(),
                });
            }
        });
    }

    // --- Sign-in panel: open and leave ---
    //
    // Both clear the last failure, so the panel never opens on an old error
    // and step 1 never keeps one (#67).
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_open_sign_in(move || {
            if let Some(app) = app_weak.upgrade() {
                log::info!("[auth] sign-in panel opened");
                clear_sign_in_error(&app);
                app.set_show_sign_in(true);
            }
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_close_sign_in(move || {
            if let Some(app) = app_weak.upgrade() {
                log::info!("[auth] sign-in panel closed — back to step 1");
                clear_sign_in_error(&app);
                app.set_show_sign_in(false);
            }
        });
    }

    // --- Sign-in panel: social auth (returning user) ---
    //
    // The panel stays open while the provider flow runs, so a failure shows
    // on it with a way forward. It used to close at once: a failed sign-in
    // then landed on step 1 with no message, and the user went round again.
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_steam(move || {
            if let Some(app) = app_weak.upgrade() {
                begin_sign_in(&app);
            }
            let _ = cmd.send(Command::AuthSteam);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_google(move || {
            if let Some(app) = app_weak.upgrade() {
                begin_sign_in(&app);
            }
            let _ = cmd.send(Command::AuthGoogle);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_twitch(move || {
            if let Some(app) = app_weak.upgrade() {
                begin_sign_in(&app);
            }
            let _ = cmd.send(Command::AuthTwitch);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_discord(move || {
            if let Some(app) = app_weak.upgrade() {
                begin_sign_in(&app);
            }
            let _ = cmd.send(Command::AuthDiscord);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_signin_apple(move || {
            if let Some(app) = app_weak.upgrade() {
                begin_sign_in(&app);
            }
            // No native Apple flow on desktop yet; empty token → handler reports unsupported.
            let _ = cmd.send(Command::AuthApple {
                identity_token: String::new(),
            });
        });
    }
}

/// A sign-in attempt starts: show progress and drop the last failure.
fn begin_sign_in(app: &crate::MainWindow) {
    app.set_login_loading(true);
    clear_sign_in_error(app);
}

fn clear_sign_in_error(app: &crate::MainWindow) {
    app.set_login_error("".into());
    app.set_login_account_missing(false);
}
