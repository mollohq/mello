use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use slint::ComponentHandle;

use super::{UpdateEvent, Updater};
use crate::ForceUpdateWindow;

const FORCE_UPDATE_WIDTH: f32 = 360.0;
const FORCE_UPDATE_HEIGHT: f32 = 188.0;

pub(crate) fn apply_renderer_override() {
    if std::env::args().any(|a| a == "--software-rendering") {
        log::info!("[startup] forcing software rendering backend");
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }
}

pub(crate) fn configure_slint_platform() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    {
        let backend = i_slint_backend_winit::Backend::builder()
            .with_default_menu_bar(false)
            .build()?;
        slint::platform::set_platform(Box::new(backend))?;
    }

    Ok(())
}

pub(crate) fn run_gate(
    updater: Rc<RefCell<Option<Updater>>>,
    update_event_rx: Receiver<UpdateEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let dialog = ForceUpdateWindow::new()?;
    let (current_version, target_version, total_bytes) = {
        let updater_ref = updater.borrow();
        let updater = updater_ref.as_ref().ok_or("Updater unavailable")?;
        (
            updater.current_version(),
            updater.target_version().unwrap_or_default(),
            updater.target_download_size().unwrap_or_default(),
        )
    };

    dialog.set_current_version(current_version.into());
    dialog.set_target_version(target_version.into());
    dialog.set_bytes_text(format_update_bytes(0, total_bytes).into());
    dialog
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::KeepWindowShown);
    center_on_primary_screen(&dialog);

    let continue_current = Rc::new(Cell::new(false));
    {
        let continue_current = continue_current.clone();
        dialog.on_start_current_clicked(move || {
            continue_current.set(true);
            slint::quit_event_loop().ok();
        });
    }
    dialog.on_quit_clicked(|| {
        std::process::exit(0);
    });
    {
        let updater = updater.clone();
        let dialog_weak = dialog.as_weak();
        dialog.on_retry_clicked(move || {
            if let Some(dialog) = dialog_weak.upgrade() {
                start_update(&updater, &dialog);
            }
        });
    }

    let event_rx = Rc::new(update_event_rx);
    let dialog_weak = dialog.as_weak();
    let poll_timer = slint::Timer::default();
    poll_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            let Some(dialog) = dialog_weak.upgrade() else {
                return;
            };

            while let Ok(event) = event_rx.try_recv() {
                match event {
                    UpdateEvent::DownloadStarted { total_bytes } => {
                        dialog.set_failed(false);
                        dialog.set_stage("Downloading".into());
                        dialog.set_progress(0.0);
                        dialog.set_progress_text("0%".into());
                        dialog.set_bytes_text(format_update_bytes(0, total_bytes).into());
                    }
                    UpdateEvent::DownloadProgress {
                        progress,
                        downloaded_bytes,
                        total_bytes,
                    } => {
                        dialog.set_stage("Downloading".into());
                        dialog.set_progress(progress);
                        dialog.set_progress_text(format_update_percent(progress).into());
                        dialog.set_bytes_text(
                            format_update_bytes(downloaded_bytes, total_bytes).into(),
                        );
                    }
                    UpdateEvent::DownloadComplete => {
                        dialog.set_stage("Verifying".into());
                        dialog.set_progress(-1.0);
                    }
                    UpdateEvent::ApplyStarted => {
                        dialog.set_stage("Restarting".into());
                        dialog.set_progress(1.0);
                        dialog.set_progress_text("100%".into());
                    }
                    UpdateEvent::Error(message) => {
                        show_failure(&dialog, message);
                    }
                    UpdateEvent::CheckStarted | UpdateEvent::CheckComplete { .. } => {}
                }
            }
        },
    );

    dialog.show()?;
    start_update(&updater, &dialog);
    slint::run_event_loop_until_quit()?;
    poll_timer.stop();
    dialog.hide()?;

    if continue_current.get() {
        Ok(())
    } else {
        Err("Force update window closed unexpectedly".into())
    }
}

#[cfg(target_os = "windows")]
fn center_on_primary_screen(dialog: &ForceUpdateWindow) {
    use slint::PhysicalPosition;
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    let scale = dialog.window().scale_factor();
    let width = (FORCE_UPDATE_WIDTH * scale).round() as i32;
    let height = (FORCE_UPDATE_HEIGHT * scale).round() as i32;
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };

    if screen_width > 0 && screen_height > 0 {
        dialog.window().set_position(PhysicalPosition::new(
            (screen_width - width) / 2,
            (screen_height - height) / 2,
        ));
    }
}

#[cfg(not(target_os = "windows"))]
fn center_on_primary_screen(_dialog: &ForceUpdateWindow) {}

fn format_update_bytes(downloaded: u64, total: u64) -> String {
    const MB: f64 = 1_048_576.0;
    format!(
        "{:.1} / {:.1} MB",
        downloaded as f64 / MB,
        total as f64 / MB
    )
}

fn format_update_percent(progress: f32) -> String {
    format!("{}%", (progress.clamp(0.0, 1.0) * 100.0).round() as u32)
}

fn update_error_code(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        "E_NET_TIMEOUT"
    } else if lower.contains("network") || lower.contains("connection") {
        "E_NET"
    } else {
        "E_UPDATE_FAILED"
    }
}

fn show_failure(dialog: &ForceUpdateWindow, message: String) {
    dialog.set_failed(true);
    dialog.set_hard_failure(false);
    dialog.set_progress(0.34);
    dialog.set_error_code(update_error_code(&message).into());
    dialog.set_error_message(message.into());
}

fn reset_progress(dialog: &ForceUpdateWindow) {
    dialog.set_failed(false);
    dialog.set_hard_failure(false);
    dialog.set_stage("Downloading".into());
    dialog.set_progress(0.0);
    dialog.set_progress_text("0%".into());
    dialog.set_error_code("".into());
    dialog.set_error_message("".into());
}

fn start_update(updater: &Rc<RefCell<Option<Updater>>>, dialog: &ForceUpdateWindow) {
    reset_progress(dialog);

    let result = updater
        .borrow_mut()
        .as_mut()
        .map(|updater| updater.update_and_restart())
        .unwrap_or_else(|| Err("Updater unavailable".into()));

    if let Err(e) = result {
        show_failure(dialog, e.to_string());
    }
}
