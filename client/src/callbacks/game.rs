use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::MainWindow;

pub fn wire(ctx: &AppContext) {
    // Reaction tapped (win/loss/highlight)
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_reaction_tapped(move |sentiment| {
            let sentiment = sentiment.to_string();

            if sentiment == "highlight" {
                if let Some(app) = app_weak.upgrade() {
                    app.set_bar_state(3);
                }
                return;
            }

            let game_name = app_weak
                .upgrade()
                .map(|a: MainWindow| a.get_game_name().to_string())
                .unwrap_or_default();

            let _ = cmd.try_send(Command::PostMoment {
                crew_id: String::new(),
                sentiment,
                text: String::new(),
                game_name,
            });

            if let Some(app) = app_weak.upgrade() {
                app.set_bar_state(4);
                start_confirmed_timer(app.as_weak());
            }
        });
    }

    // Moment submitted (text highlight)
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_moment_submitted(move |text| {
            let game_name = app_weak
                .upgrade()
                .map(|a: MainWindow| a.get_game_name().to_string())
                .unwrap_or_default();

            let _ = cmd.try_send(Command::PostMoment {
                crew_id: String::new(),
                sentiment: "highlight".into(),
                text: text.to_string(),
                game_name,
            });

            if let Some(app) = app_weak.upgrade() {
                app.set_bar_state(4);
                start_confirmed_timer(app.as_weak());
            }
        });
    }

    // Moment dismissed
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_moment_dismissed(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_game_active(false);
                app.set_bar_state(0);
            }
        });
    }

    // Stream requested
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let fg_monitor = ctx.fg_monitor.clone();
        ctx.app.on_stream_requested(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            if app.get_is_hosting() {
                return;
            }

            let crew_id = app.get_active_crew_id().to_string();
            if crew_id.is_empty() {
                log::warn!("[ui] stream requested, but no active crew selected");
                return;
            }

            let pid = fg_monitor.borrow().game_pid();
            let Some(game_pid) = pid else {
                log::warn!("[ui] stream requested, but no detected game PID is available");
                return;
            };

            let mut title = app.get_game_name().to_string();
            if title.trim().is_empty() {
                title = "STREAMING".to_string();
            }
            app.set_stream_label(title.clone().into());

            log::info!(
                "[ui] quick stream start: crew={} game_pid={} title={}",
                crew_id,
                game_pid,
                title
            );
            let _ = cmd.try_send(Command::StartStream {
                crew_id,
                title,
                capture_mode: "process".to_string(),
                monitor_index: None,
                hwnd: None,
                pid: Some(game_pid),
                preset: 2, // Medium
            });
        });
    }
}

fn start_confirmed_timer(app_weak: slint::Weak<MainWindow>) {
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_secs(3),
        move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_game_active(false);
                app.set_bar_state(0);
            }
        },
    );
    std::mem::forget(timer);
}
