pub mod renderer;
pub mod window;

use crate::protocol::HudState;

/// The D2D overlay window — transparent, click-through, composited by DWM.
/// Created at startup, shown/hidden by the mode manager.
pub struct OverlayWindow {
    #[cfg(target_os = "windows")]
    inner: window::Win32OverlayWindow,
}

impl OverlayWindow {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        #[cfg(target_os = "windows")]
        {
            let inner = window::Win32OverlayWindow::new()?;
            Ok(Self { inner })
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err("Overlay is Windows-only".into())
        }
    }

    pub fn show(&self) {
        #[cfg(target_os = "windows")]
        self.inner.show();
    }

    pub fn hide(&self) {
        #[cfg(target_os = "windows")]
        self.inner.hide();
    }

    pub fn update_state(&mut self, state: &HudState) {
        #[cfg(target_os = "windows")]
        self.inner.update_state(state);
    }

    pub fn render(&mut self) {
        #[cfg(target_os = "windows")]
        self.inner.render();
    }

    pub fn ensure_topmost(&self) {
        #[cfg(target_os = "windows")]
        self.inner.ensure_topmost();
    }
}
