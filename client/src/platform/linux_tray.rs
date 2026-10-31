//! The Linux tray: a StatusNotifierItem through `ksni`.
//!
//! KDE shows StatusNotifierItems natively. GNOME shows them only with the
//! AppIndicator extension. `tray-icon` builds its Linux menu with GTK, which
//! needs a GTK main loop that the client does not run, so Linux uses this
//! instead.
//!
//! Threads: `ksni` runs the D-Bus service on its own thread. Menu callbacks
//! run there and only send a menu id into a channel; the poll loop reads the
//! channel on the UI thread and handles the ids like the `tray-icon` menu
//! (`poll_loop::handle_menu_item`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Icon, MenuItem, OfflineReason, ToolTip, Tray};

use super::VoiceState;
use crate::{APP_NAME, LINUX_APP_ID};

/// The id of a left click on the tray icon: show or hide the window.
pub const TOGGLE_WINDOW: &str = "tray_toggle";

struct M3lloTray {
    state: VoiceState,
    muted: bool,
    tx: mpsc::Sender<String>,
    watcher_online: Arc<AtomicBool>,
}

fn send(id: &'static str) -> Box<dyn Fn(&mut M3lloTray) + Send> {
    Box::new(move |tray: &mut M3lloTray| {
        let _ = tray.tx.send(id.to_string());
    })
}

impl Tray for M3lloTray {
    fn id(&self) -> String {
        LINUX_APP_ID.into()
    }

    fn title(&self) -> String {
        APP_NAME.into()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: APP_NAME.into(),
            ..Default::default()
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![argb_icon(self.state)]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(TOGGLE_WINDOW.to_string());
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let in_voice = matches!(
            self.state,
            VoiceState::Connected | VoiceState::Speaking | VoiceState::Muted
        );
        vec![
            StandardItem {
                label: format!("Open {APP_NAME}"),
                activate: send("tray_open"),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Mute".into(),
                enabled: in_voice,
                checked: self.muted,
                activate: send("tray_mute"),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Leave Voice".into(),
                enabled: in_voice,
                activate: send("tray_leave"),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: format!("Quit {APP_NAME}"),
                activate: send("tray_quit"),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_online(&self) {
        log::info!("[tray] StatusNotifier host online");
        self.watcher_online.store(true, Ordering::Relaxed);
    }

    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        log::info!("[tray] no StatusNotifier host ({reason:?})");
        self.watcher_online.store(false, Ordering::Relaxed);
        // Keep the service: a host that starts later picks the tray up.
        true
    }
}

/// The running tray service and the channel of its menu ids.
pub struct LinuxTray {
    handle: Handle<M3lloTray>,
    rx: mpsc::Receiver<String>,
    watcher_online: Arc<AtomicBool>,
}

impl LinuxTray {
    /// Errors when no StatusNotifierWatcher exists (GNOME without the
    /// AppIndicator extension): ksni's spawn fails then.
    pub fn spawn(state: VoiceState) -> Result<Self, Box<dyn std::error::Error>> {
        let (tx, rx) = mpsc::channel();
        // True from the start: a spawn that returns Ok has registered the item
        // with a watcher. ksni calls watcher_online only when a watcher comes
        // back after watcher_offline, never for the first registration
        // (ksni 0.3.7, service.rs), so a false start would never turn true.
        let watcher_online = Arc::new(AtomicBool::new(true));
        let handle = M3lloTray {
            state,
            muted: false,
            tx,
            watcher_online: watcher_online.clone(),
        }
        .spawn()?;
        Ok(Self {
            handle,
            rx,
            watcher_online,
        })
    }

    /// True while a StatusNotifier host shows the tray (the KDE panel, or
    /// GNOME with the AppIndicator extension).
    pub fn is_shown(&self) -> bool {
        self.watcher_online.load(Ordering::Relaxed)
    }

    pub fn set_state(&self, state: VoiceState, muted: bool) {
        self.handle.update(|tray| {
            tray.state = state;
            tray.muted = muted;
        });
    }

    pub fn set_muted(&self, muted: bool) {
        self.handle.update(|tray| tray.muted = muted);
    }

    /// The menu ids since the last call. UI thread.
    pub fn poll(&self) -> Vec<String> {
        self.rx.try_iter().collect()
    }
}

/// The state circle in ARGB32, network byte order, as StatusNotifierItem
/// pixmaps want it. Same pixels as the tray-icon tray.
fn argb_icon(state: VoiceState) -> Icon {
    let (rgba, size) = super::state_icon_rgba(state);
    let mut data = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        data.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
    }
    Icon {
        width: size as i32,
        height: size as i32,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_argb_in_network_order() {
        let icon = argb_icon(VoiceState::Muted);
        let (rgba, size) = super::super::state_icon_rgba(VoiceState::Muted);
        assert_eq!(icon.width as u32, size);
        assert_eq!(icon.data.len(), rgba.len());
        // The center pixel: RGBA (255, 68, 68, 255) becomes ARGB.
        let center = ((size / 2) * size + size / 2) as usize * 4;
        assert_eq!(&rgba[center..center + 4], &[255, 68, 68, 255]);
        assert_eq!(&icon.data[center..center + 4], &[255, 255, 68, 68]);
    }
}
