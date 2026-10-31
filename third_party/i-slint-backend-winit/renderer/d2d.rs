// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Delegate the rendering to mello's Direct2D renderer (`renderer-d2d`;
//! m3llo-fx-lab D2D-RENDERER.md). Windows only. Selected with the renderer
//! name `d2d` (`MELLO_RENDERER=d2d`).

use std::rc::Rc;
use std::sync::Arc;

use crate::winit_compat::WindowSurfaceSizeExt;
use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::DrawOutcome;
use i_slint_renderer_d2d::D2DRenderer;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::event_loop::ActiveEventLoop;

pub struct WinitD2DRenderer {
    renderer: D2DRenderer,
}

impl WinitD2DRenderer {
    pub fn new_suspended(
        _shared_backend_data: &Rc<crate::SharedBackendData>,
    ) -> Result<Box<dyn super::WinitCompatibleRenderer>, PlatformError> {
        Ok(Box::new(Self { renderer: D2DRenderer::new() }))
    }
}

impl super::WinitCompatibleRenderer for WinitD2DRenderer {
    fn render(&self, _window: &i_slint_core::api::Window) -> Result<DrawOutcome, PlatformError> {
        self.renderer.render()
    }

    fn as_core_renderer(&self) -> &dyn i_slint_core::renderer::Renderer {
        &self.renderer
    }

    fn suspend(&self) -> Result<(), PlatformError> {
        self.renderer.suspend()
    }

    fn resume(
        &self,
        active_event_loop: &ActiveEventLoop,
        window_attributes: winit::window::WindowAttributes,
        _window_adapter_weak: std::rc::Weak<crate::winitwindowadapter::WinitWindowAdapter>,
    ) -> Result<Arc<winit::window::Window>, PlatformError> {
        let winit_window = Arc::new(active_event_loop.create_window(window_attributes).map_err(
            |winit_os_error| {
                PlatformError::from(format!(
                    "Error creating native window for Direct2D rendering: {winit_os_error}"
                ))
            },
        )?);

        let hwnd = match winit_window
            .window_handle()
            .map_err(|e| PlatformError::from(format!("d2d renderer: window handle: {e}")))?
            .as_raw()
        {
            RawWindowHandle::Win32(handle) => handle.hwnd.get(),
            _ => return Err("d2d renderer: not a Win32 window".into()),
        };
        let size = winit_window.surface_size();
        self.renderer.set_window(
            hwnd,
            size.width,
            size.height,
            Box::new({
                let winit_window = winit_window.clone();
                move || winit_window.pre_present_notify()
            }),
        )?;
        Ok(winit_window)
    }
}
