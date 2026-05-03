#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_context;
mod autolaunch;
mod avatar;
mod callbacks;
mod converters;
#[cfg(target_os = "windows")]
pub mod dcomp_presenter;
mod deep_link;
mod foreground_monitor;
mod gif_animator;
mod handlers;
pub mod hud_manager;
mod hud_state_builder;
mod image_cache;
mod ipc;
mod notifications;
mod platform;
mod poll_loop;
mod settings;
mod updater;

pub const APP_NAME: &str = "m3llo";

slint::include_modules!();

use mello_core::{Client, Command, Config, Event, FrameLifecycleSlot, FRAME_STATE_PRESENTED};
use platform::StatusItem;
use settings::Settings;
use slint::{ComponentHandle, Model};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;
use updater::{UpdateEvent, Updater};

use single_instance::SingleInstance;

const HISTORY_LEN: usize = 30;

pub struct DebugHistory {
    levels: [f32; HISTORY_LEN],
    speaking: [bool; HISTORY_LEN],
    cursor: usize,
}

impl DebugHistory {
    fn new() -> Self {
        Self {
            levels: [0.0; HISTORY_LEN],
            speaking: [false; HISTORY_LEN],
            cursor: 0,
        }
    }

    fn push(&mut self, level: f32, spk: bool) {
        self.levels[self.cursor] = level;
        self.speaking[self.cursor] = spk;
        self.cursor = (self.cursor + 1) % HISTORY_LEN;
    }

    pub fn get(&self, i: usize) -> (f32, bool) {
        let idx = (self.cursor + i) % HISTORY_LEN;
        (self.levels[idx], self.speaking[idx])
    }
}

fn nakama_config() -> Config {
    #[cfg(feature = "production")]
    return Config::production();

    #[cfg(not(feature = "production"))]
    Config::development()
}

fn init_logging() -> Option<std::path::PathBuf> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stderr_layer = fmt::layer().with_target(true).with_writer(std::io::stderr);

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer);

    if let Some(data_dir) = directories::ProjectDirs::from("app", "mello", "mello") {
        let log_dir = data_dir.data_dir().join("logs");
        if std::fs::create_dir_all(&log_dir).is_ok() {
            let file_appender = tracing_appender::rolling::daily(&log_dir, "mello.log");
            let file_layer = fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(file_appender);
            registry.with(file_layer).init();
            return Some(log_dir);
        }
    }

    registry.init();
    None
}

fn main() {
    Updater::run_lifecycle_hooks();

    let log_dir = init_logging();

    std::panic::set_hook(Box::new(|info| {
        log::error!("PANIC: {}", info);
    }));

    #[cfg(target_os = "windows")]
    {
        if let Some(ref dir) = log_dir {
            platform::crash_handler::set_log_dir(dir.clone());
        }
        platform::crash_handler::install();
    }

    let _ = log_dir;

    if let Err(e) = run_app() {
        log::error!("Fatal: {}", e);
        std::process::exit(1);
    }
}

fn run_app() -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Starting Mello...");

    // --- Single instance enforcement ---
    // single-instance on macOS uses the name as a file path (cwd-relative),
    // so we place the lock file in a stable writable location.
    let instance_suffix = std::env::args()
        .position(|a| a == "--instance")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_default();
    let lock_name = if instance_suffix.is_empty() {
        "app.mello.desktop".to_string()
    } else {
        format!("app.mello.desktop.{}", instance_suffix)
    };
    // macOS: single-instance uses the name as a cwd-relative file path,
    // so we must give it an absolute path in a writable location.
    // Windows/Linux: name is used as a mutex/socket name, no path needed.
    #[cfg(target_os = "macos")]
    let instance_id = std::env::temp_dir()
        .join(&lock_name)
        .to_string_lossy()
        .to_string();
    #[cfg(not(target_os = "macos"))]
    let instance_id = lock_name.clone();
    let ipc_endpoint = ipc::endpoint_name(&lock_name);

    let _instance = SingleInstance::new(&instance_id)?;
    if !_instance.is_single() {
        if let Some(url) = std::env::args()
            .nth(1)
            .filter(|a| a.starts_with("mello://"))
        {
            if ipc::send_to_running(&ipc_endpoint, &url) {
                eprintln!("Relayed deep link to running instance: {url}");
            } else {
                eprintln!("Mello is already running. Could not relay deep link: {url}");
            }
        } else {
            eprintln!("Mello is already running.");
        }
        std::process::exit(0);
    }

    let ipc_listener = match ipc::IpcListener::bind(&ipc_endpoint) {
        Ok(l) => Some(l),
        Err(e) => {
            log::warn!("[ipc] failed to bind listener: {}", e);
            None
        }
    };

    // --- Deep link from argv ---
    let pending_deep_link = deep_link::extract_deep_link().and_then(|url| {
        let link = deep_link::parse(&url);
        if let Some(ref l) = link {
            log::info!("Deep link parsed: {:?}", l);
        }
        link
    });

    let loopback = std::env::args().any(|a| a == "--loopback");

    if std::env::args().any(|a| a == "--reset") {
        log::info!("--reset flag detected, wiping all settings");
        Settings::default().save();
    }

    let rt = tokio::runtime::Runtime::new()?;

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<Command>(256);
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();

    // --- Auto-updater ---
    let (update_event_tx, update_event_rx) = std::sync::mpsc::channel::<UpdateEvent>();
    let updater: Rc<RefCell<Option<Updater>>> =
        Rc::new(RefCell::new(match Updater::new(update_event_tx) {
            Ok(u) => {
                log::info!("Updater ready ÔÇö v{}", u.current_version());
                Some(u)
            }
            Err(e) => {
                log::warn!("Updater init failed (dev mode?): {}", e);
                None
            }
        }));

    if let Some(ref mut u) = *updater.borrow_mut() {
        u.check_for_updates();
    }

    let frame_slot: mello_core::FrameSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let native_frame_slot: mello_core::NativeFrameSlot =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let frame_consumed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let frame_lifecycle: FrameLifecycleSlot =
        std::sync::Arc::new(std::sync::atomic::AtomicU8::new(FRAME_STATE_PRESENTED));
    let frame_slot_for_client = frame_slot.clone();
    let native_frame_slot_for_client = native_frame_slot.clone();
    let frame_consumed_for_client = frame_consumed.clone();
    let frame_lifecycle_for_client = frame_lifecycle.clone();

    rt.spawn(async move {
        let mut client = Client::new(
            nakama_config(),
            event_tx,
            loopback,
            frame_slot_for_client,
            native_frame_slot_for_client,
            frame_consumed_for_client,
            frame_lifecycle_for_client,
        );
        client.run(cmd_rx).await;
    });

    if std::env::args().any(|a| a == "--software-rendering") {
        log::info!("[startup] forcing software rendering backend");
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }

    // --- macOS: disable Slint's default menu bar ---
    #[cfg(target_os = "macos")]
    {
        let backend = i_slint_backend_winit::Backend::builder()
            .with_default_menu_bar(false)
            .build()?;
        slint::platform::set_platform(Box::new(backend))?;
    }

    let app = MainWindow::new()?;
    app.set_settings_build_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());

    #[cfg(target_os = "windows")]
    let dcomp_presenter: Rc<RefCell<Option<dcomp_presenter::DCompPresenter>>> =
        Rc::new(RefCell::new(None));

    // --- macOS native menu bar ---
    #[cfg(target_os = "macos")]
    let _menu_bar = {
        let menu = platform::macos::build_menu_bar();
        menu.init_for_nsapp();
        menu
    };

    // --- Tray / status item ---
    let status_item = Rc::new(RefCell::new(
        StatusItem::new().expect("failed to create tray icon"),
    ));

    // --- Global hotkey manager ---
    let hotkey_mgr = Rc::new(RefCell::new(
        platform::hotkeys::HotkeyManager::new().expect("failed to init hotkey manager"),
    ));

    let settings = Rc::new(RefCell::new(Settings::load()));

    // --- Close ÔåÆ tray ---
    {
        let window_ref = app.as_weak();
        let s = settings.clone();
        app.window().on_close_requested(move || {
            if s.borrow().close_to_tray {
                if let Some(w) = window_ref.upgrade() {
                    w.hide().ok();
                }
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                log::info!("[quit] close requested (close_to_tray=false)");
                slint::quit_event_loop().ok();
                slint::CloseRequestResponse::KeepWindowShown
            }
        });
    }

    app.global::<Theme>().set_dark(settings.borrow().dark_theme);

    // Apply saved audio device and processing settings
    {
        let s = settings.borrow();
        if let Some(ref id) = s.capture_device_id {
            let _ = cmd_tx.try_send(Command::SetCaptureDevice { id: id.clone() });
        }
        if let Some(ref id) = s.playback_device_id {
            let _ = cmd_tx.try_send(Command::SetPlaybackDevice { id: id.clone() });
        }
        let _ = cmd_tx.try_send(Command::SetEchoCancellation {
            enabled: s.echo_cancellation,
        });
        let _ = cmd_tx.try_send(Command::SetNoiseSuppression {
            enabled: s.noise_suppression,
        });
        let _ = cmd_tx.try_send(Command::SetInputVolume {
            volume: s.input_volume,
        });
        let _ = cmd_tx.try_send(Command::SetOutputVolume {
            volume: s.output_volume,
        });
    }

    // Restore saved PTT binding and activate listener if PTT mode is on
    {
        let s = settings.borrow();
        if let Some(ref key_str) = s.ptt_key {
            if let Some((binding, label)) = platform::hotkeys::parse_ptt_string(key_str) {
                log::info!("Restored PTT key: {}", label);
                hotkey_mgr.borrow().register_ptt(binding);
            }
        }
        let ptt_active = s.input_mode == "push_to_talk";
        hotkey_mgr.borrow().set_active(ptt_active);
        if ptt_active {
            let _ = cmd_tx.try_send(Command::SetMute { muted: true });
        }
    }

    // Decide startup path
    {
        let s = settings.borrow();
        log::info!("[auth] startup  onboarding_step={}", s.onboarding_step);
        if s.onboarding_step > 3 {
            log::info!("[auth] onboarding done ÔÇö attempting session restore");
            let _ = cmd_tx.try_send(Command::TryRestore);
        } else {
            log::info!("[auth] onboarding in progress ÔÇö fetching crews (no auth)");
            let _ = cmd_tx.try_send(Command::DiscoverCrews { cursor: None });
        }
        app.set_onboarding_step(s.onboarding_step as i32);
        let _ = cmd_tx.try_send(Command::CheckMicPermission);
    }

    // --- HUD manager ---
    let hud_enabled = settings.borrow().hud_enabled;
    let hud_mgr = Rc::new(hud_manager::HudManager::start(hud_enabled));
    if hud_enabled {
        let s = settings.borrow();
        hud_mgr.push_settings(hud_manager::HudSettings {
            overlay_opacity: s.hud_overlay_opacity,
            show_clip_toasts: s.hud_show_clip_toasts,
            overlay_enabled: s.hud_show_overlay_in_game,
        });
    }
    let fg_monitor = Rc::new(RefCell::new(foreground_monitor::ForegroundMonitor::new(
        hud_enabled,
        settings.borrow().hud_show_overlay_in_game,
    )));

    // --- GIF animators ---
    let gif_popover_anim = gif_animator::GifAnimator::new(50, None);
    let gif_chat_anim = gif_animator::GifAnimator::new(50, Some(2));

    // --- Build AppContext ---
    let ctx = app_context::AppContext {
        app,
        cmd_tx,
        settings,
        rt: rt.handle().clone(),
        active_voice_channel: Rc::new(RefCell::new(String::new())),
        new_crew_avatar_b64: std::sync::Arc::new(std::sync::Mutex::new(None)),
        invited_users: Rc::new(RefCell::new(Vec::new())),
        discover_cursor: Rc::new(RefCell::new(None)),
        discover_loading: Rc::new(RefCell::new(false)),
        chat_messages: Rc::new(RefCell::new(Vec::new())),
        avatar_state: std::sync::Arc::new(std::sync::Mutex::new(avatar::AvatarGridState::new())),
        profile_avatar_state: std::sync::Arc::new(std::sync::Mutex::new(
            avatar::AvatarGridState::new(),
        )),
        avatar_shuffle_timer: Rc::new(RefCell::new(None)),
        muted_before_deafen: Rc::new(Cell::new(false)),
        updater,
        hotkey_mgr,
        status_item,
        gif_popover_anim,
        gif_chat_anim,
        dbg_hist: Rc::new(RefCell::new(DebugHistory::new())),
        avatar_cache: Rc::new(RefCell::new(std::collections::HashMap::new())),
        hud_manager: hud_mgr,
        fg_monitor,
        pending_deep_link: Rc::new(RefCell::new(pending_deep_link)),
        ipc_listener: Rc::new(RefCell::new(ipc_listener)),
        #[cfg(target_os = "windows")]
        native_frame_slot: native_frame_slot.clone(),
        #[cfg(target_os = "windows")]
        frame_lifecycle: frame_lifecycle.clone(),
        #[cfg(target_os = "windows")]
        dcomp_presenter: dcomp_presenter.clone(),
    };

    // --- Wire all callbacks ---
    callbacks::wire_all(&ctx);
    log::info!("[startup] callbacks wired");

    // --- Wire DComp geometry callback (Slint → Rust, synchronous) ---
    #[cfg(target_os = "windows")]
    {
        let geo_dcomp = dcomp_presenter.clone();
        let geo_app_weak = ctx.app.as_weak();
        ctx.app.global::<VideoRect>().on_geometry_changed(
            move |canvas_x, canvas_y, canvas_w, canvas_h, viewport_y, viewport_h| {
                let scale = geo_app_weak
                    .upgrade()
                    .map(|a| a.window().scale_factor())
                    .unwrap_or(1.0);
                if let Some(ref presenter) = *geo_dcomp.borrow() {
                    presenter.update_geometry(
                        canvas_x * scale,
                        canvas_y * scale,
                        canvas_w * scale,
                        canvas_h * scale,
                        viewport_y * scale,
                        viewport_h * scale,
                    );
                }
            },
        );
    }

    // --- Start chat GIF animator ---
    {
        let app_weak = ctx.app.as_weak();
        ctx.gif_chat_anim.start(move |url, img| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let msgs = app.get_messages();
            for i in 0..msgs.row_count() {
                if let Some(mut m) = msgs.row_data(i) {
                    if m.is_gif && m.gif_preview_url.as_str() == url {
                        m.gif_image = img.clone();
                        m.has_gif_image = true;
                        msgs.set_row_data(i, m);
                    }
                }
            }
        });
    }

    // --- Start poll loop ---
    let _poll_timer = poll_loop::start(&ctx, event_rx, update_event_rx);
    log::info!("[startup] poll loop started");

    // --- 16ms frame timer for stream display ---
    let frame_app_weak = ctx.app.as_weak();
    let frame_timer = slint::Timer::default();
    let mut frame_timer_ticks: u64 = 0;
    let mut frame_timer_last_log = Instant::now();
    let mut frame_timer_presented: u64 = 0;
    #[cfg(target_os = "windows")]
    let mut last_surface_sequence: u64 = 0;
    #[cfg(target_os = "windows")]
    let frame_timer_dcomp = dcomp_presenter.clone();
    frame_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(16),
        move || {
            let app_for_tick = frame_app_weak.upgrade();
            let is_watching = app_for_tick
                .as_ref()
                .map(|app| app.get_is_watching())
                .unwrap_or(false);
            if !is_watching {
                if let Some(app) = app_for_tick.as_ref() {
                    app.set_dbg_stream_ui_render_fps(0.0);
                }
                return;
            }

            frame_timer_ticks = frame_timer_ticks.saturating_add(1);

            #[cfg(target_os = "windows")]
            if let Ok(slot) = native_frame_slot.lock() {
                if let Some(frame) = *slot {
                    if frame.sequence != last_surface_sequence {
                        last_surface_sequence = frame.sequence;
                        if let Some(ref mut presenter) = *frame_timer_dcomp.borrow_mut() {
                            if presenter.present_shared_texture(
                                frame.shared_handle,
                                frame.width,
                                frame.height,
                            ) {
                                frame_timer_presented = frame_timer_presented.saturating_add(1);
                            }
                        }
                    }
                }
            }

            #[cfg(not(target_os = "windows"))]
            {
                let frame_data = frame_slot.lock().ok().and_then(|mut s| s.take());
                if let Some((w, h, rgba)) = frame_data {
                    if let Some(app) = app_for_tick.as_ref() {
                        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            &rgba, w, h,
                        );
                        app.set_stream_frame(slint::Image::from_rgba8(buf));
                        frame_timer_presented = frame_timer_presented.saturating_add(1);
                    }
                }
            }

            frame_consumed.store(true, std::sync::atomic::Ordering::Release);
            frame_lifecycle.store(FRAME_STATE_PRESENTED, std::sync::atomic::Ordering::Release);

            if frame_timer_last_log.elapsed().as_secs_f32() >= 1.0 {
                let elapsed = frame_timer_last_log.elapsed().as_secs_f32().max(0.001);
                let present_fps = frame_timer_presented as f32 / elapsed;

                if let Some(app) = app_for_tick.as_ref() {
                    app.set_dbg_stream_ui_render_fps(present_fps);
                }

                #[cfg(target_os = "windows")]
                log::info!(
                    "DComp stream: present_fps={:.1} tick_hz={:.1}",
                    present_fps,
                    frame_timer_ticks as f32 / elapsed
                );
                #[cfg(not(target_os = "windows"))]
                log::info!(
                    "Stream: present_fps={:.1} tick_hz={:.1}",
                    present_fps,
                    frame_timer_ticks as f32 / elapsed
                );

                frame_timer_ticks = 0;
                frame_timer_presented = 0;
                frame_timer_last_log = Instant::now();
            }
        },
    );

    ctx.app.show()?;
    log::info!("[startup] window shown");
    slint::run_event_loop_until_quit()?;
    log::info!("[exit] event loop ended");

    ctx.hud_manager.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn init_test_backend() {
        i_slint_backend_testing::init_no_event_loop();
    }

    fn wire_mic_toggle(app: &MainWindow, sent: Rc<Cell<Option<bool>>>) {
        let app_weak = app.as_weak();
        app.on_mic_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_muted = !app.get_mic_muted();
                app.set_mic_muted(new_muted);
                sent.set(Some(new_muted));
            }
        });
    }

    fn wire_deafen_toggle(
        app: &MainWindow,
        sent_deafened: Rc<Cell<Option<bool>>>,
        sent_muted: Rc<Cell<Option<bool>>>,
        mbd: Rc<Cell<bool>>,
    ) {
        let app_weak = app.as_weak();
        app.on_deafen_toggle(move || {
            if let Some(app) = app_weak.upgrade() {
                let new_deafened = !app.get_deafened();
                app.set_deafened(new_deafened);
                sent_deafened.set(Some(new_deafened));

                if new_deafened {
                    mbd.set(app.get_mic_muted());
                    if !app.get_mic_muted() {
                        app.set_mic_muted(true);
                        sent_muted.set(Some(true));
                    }
                } else {
                    if !mbd.get() {
                        app.set_mic_muted(false);
                        sent_muted.set(Some(false));
                    }
                }
            }
        });
    }

    #[test]
    fn mic_toggle_sends_correct_muted_state() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent = Rc::new(Cell::new(None::<bool>));
        wire_mic_toggle(&app, sent.clone());

        assert!(!app.get_mic_muted(), "should start unmuted");

        app.invoke_mic_toggle();
        assert_eq!(sent.get(), Some(true), "first toggle ÔåÆ muted=true");
        assert!(app.get_mic_muted());

        app.invoke_mic_toggle();
        assert_eq!(sent.get(), Some(false), "second toggle ÔåÆ muted=false");
        assert!(!app.get_mic_muted());
    }

    #[test]
    fn deafen_toggle_sends_correct_deafened_state() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent_d = Rc::new(Cell::new(None::<bool>));
        let sent_m = Rc::new(Cell::new(None::<bool>));
        let mbd = Rc::new(Cell::new(false));
        wire_deafen_toggle(&app, sent_d.clone(), sent_m.clone(), mbd);

        assert!(!app.get_deafened(), "should start undeafened");

        app.invoke_deafen_toggle();
        assert_eq!(sent_d.get(), Some(true));
        assert!(app.get_deafened());

        app.invoke_deafen_toggle();
        assert_eq!(sent_d.get(), Some(false));
        assert!(!app.get_deafened());
    }

    #[test]
    fn deafen_auto_mutes_when_unmuted() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent_d = Rc::new(Cell::new(None::<bool>));
        let sent_m = Rc::new(Cell::new(None::<bool>));
        let mbd = Rc::new(Cell::new(false));
        wire_deafen_toggle(&app, sent_d.clone(), sent_m.clone(), mbd);

        assert!(!app.get_mic_muted());

        app.invoke_deafen_toggle();
        assert!(app.get_deafened());
        assert!(app.get_mic_muted(), "deafen should auto-mute");
        assert_eq!(sent_m.get(), Some(true), "SetMute(true) should be sent");
    }

    #[test]
    fn undeafen_restores_unmuted_when_was_not_muted() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent_d = Rc::new(Cell::new(None::<bool>));
        let sent_m = Rc::new(Cell::new(None::<bool>));
        let mbd = Rc::new(Cell::new(false));
        wire_deafen_toggle(&app, sent_d.clone(), sent_m.clone(), mbd);

        app.invoke_deafen_toggle();
        assert!(app.get_mic_muted());

        sent_m.set(None);
        app.invoke_deafen_toggle();
        assert!(!app.get_deafened());
        assert!(
            !app.get_mic_muted(),
            "un-deafen should restore unmuted state"
        );
        assert_eq!(sent_m.get(), Some(false));
    }

    #[test]
    fn undeafen_keeps_muted_when_was_manually_muted() {
        init_test_backend();
        let app = MainWindow::new().unwrap();
        let sent_mic = Rc::new(Cell::new(None::<bool>));
        let sent_d = Rc::new(Cell::new(None::<bool>));
        let sent_m_deafen = Rc::new(Cell::new(None::<bool>));
        let mbd = Rc::new(Cell::new(false));

        wire_mic_toggle(&app, sent_mic.clone());
        wire_deafen_toggle(&app, sent_d.clone(), sent_m_deafen.clone(), mbd);

        app.invoke_mic_toggle();
        assert!(app.get_mic_muted());

        sent_m_deafen.set(None);
        app.invoke_deafen_toggle();
        assert!(app.get_deafened());
        assert!(app.get_mic_muted());
        assert_eq!(
            sent_m_deafen.get(),
            None,
            "no extra SetMute when already muted"
        );

        app.invoke_deafen_toggle();
        assert!(!app.get_deafened());
        assert!(
            app.get_mic_muted(),
            "should stay muted since user muted before deafen"
        );
    }
}
