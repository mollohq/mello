#![cfg(target_os = "windows")]

use std::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGISwapChain, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_PRESENT_DO_NOT_WAIT, DXGI_SWAP_CHAIN_DESC,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SW_HIDE, WINDOW_EX_STYLE, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
};

pub struct NativeStreamPresenter {
    child_hwnd: HWND,
    device: ID3D11Device,
    swap_chain: IDXGISwapChain,
    context: ID3D11DeviceContext,
    back_buffer: Option<ID3D11Texture2D>,
    backbuffer_w: u32,
    backbuffer_h: u32,
    shared_source: Option<ID3D11Texture2D>,
    shared_source_handle: usize,
    viewport_x: i32,
    viewport_y: i32,
    viewport_w: i32,
    viewport_h: i32,
    viewport_visible: bool,
}

impl NativeStreamPresenter {
    pub fn new(parent_hwnd: HWND) -> Result<Self, String> {
        unsafe {
            let child_hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                0,
                0,
                2,
                2,
                Some(parent_hwnd),
                None,
                None,
                None,
            )
            .map_err(|e| format!("CreateWindowExW failed: {}", e))?;

            let mut swap_desc = DXGI_SWAP_CHAIN_DESC::default();
            swap_desc.BufferDesc = DXGI_MODE_DESC {
                Width: 2,
                Height: 2,
                RefreshRate: DXGI_RATIONAL {
                    Numerator: 0,
                    Denominator: 1,
                },
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            };
            swap_desc.SampleDesc = DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            };
            swap_desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
            swap_desc.BufferCount = 2;
            swap_desc.OutputWindow = child_hwnd;
            swap_desc.Windowed = true.into();
            swap_desc.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;

            let mut swap_chain = None;
            let mut device = None;
            let mut context = None;
            let mut feature_level = Default::default();

            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&swap_desc),
                Some(&mut swap_chain),
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )
            .map_err(|e| format!("D3D11CreateDeviceAndSwapChain failed: {}", e))?;

            let _device = device.ok_or_else(|| "D3D11 device missing".to_string())?;
            let swap_chain = swap_chain.ok_or_else(|| "DXGI swapchain missing".to_string())?;
            let context = context.ok_or_else(|| "D3D11 context missing".to_string())?;

            Ok(Self {
                child_hwnd,
                device: _device,
                swap_chain,
                context,
                back_buffer: None,
                backbuffer_w: 0,
                backbuffer_h: 0,
                shared_source: None,
                shared_source_handle: 0,
                viewport_x: 0,
                viewport_y: 0,
                viewport_w: 0,
                viewport_h: 0,
                viewport_visible: false,
            })
        }
    }

    pub fn update_viewport(&mut self, x: i32, y: i32, width: i32, height: i32, visible: bool) {
        let geometry_changed = self.viewport_x != x
            || self.viewport_y != y
            || self.viewport_w != width
            || self.viewport_h != height;
        let visibility_changed = self.viewport_visible != visible;
        if !geometry_changed && !visibility_changed {
            return;
        }
        unsafe {
            if visible && width > 0 && height > 0 {
                let _ = SetWindowPos(
                    self.child_hwnd,
                    None,
                    x,
                    y,
                    width,
                    height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            } else {
                let _hidden = ShowWindow(self.child_hwnd, SW_HIDE).as_bool();
            }
        }
        self.viewport_x = x;
        self.viewport_y = y;
        self.viewport_w = width;
        self.viewport_h = height;
        self.viewport_visible = visible;
    }

    pub fn render_frame(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if rgba.len() != expected_len {
            return Ok(());
        }

        self.ensure_backbuffer(width, height)?;

        let Some(back_buffer) = self.back_buffer.as_ref() else {
            return Ok(());
        };

        unsafe {
            self.context.UpdateSubresource(
                back_buffer,
                0,
                None,
                rgba.as_ptr() as *const c_void,
                width * 4,
                0,
            );

            let present = self.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT);
            if let Err(e) = present.ok() {
                if e.code() != DXGI_ERROR_WAS_STILL_DRAWING {
                    return Err(format!("swapchain present failed: {}", e));
                }
            }
        }

        Ok(())
    }

    pub fn render_shared_frame(
        &mut self,
        width: u32,
        height: u32,
        shared_handle: usize,
    ) -> Result<(), String> {
        if shared_handle == 0 {
            return Ok(());
        }
        self.ensure_backbuffer(width, height)?;

        unsafe {
            if self.shared_source_handle != shared_handle || self.shared_source.is_none() {
                let mut opened = None;
                self.device
                    .OpenSharedResource(
                        HANDLE(shared_handle as *mut c_void),
                        &mut opened as *mut Option<ID3D11Texture2D>,
                    )
                    .map_err(|e| format!("OpenSharedResource failed: {}", e))?;
                self.shared_source = opened;
                self.shared_source_handle = shared_handle;
            }

            let Some(back_buffer) = self.back_buffer.as_ref() else {
                return Ok(());
            };
            let Some(shared_source) = self.shared_source.as_ref() else {
                return Ok(());
            };

            self.context.CopyResource(back_buffer, shared_source);
            let present = self.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT);
            if let Err(e) = present.ok() {
                if e.code() != DXGI_ERROR_WAS_STILL_DRAWING {
                    return Err(format!("swapchain present failed: {}", e));
                }
            }
        }
        Ok(())
    }

    fn ensure_backbuffer(&mut self, width: u32, height: u32) -> Result<(), String> {
        if self.backbuffer_w == width && self.backbuffer_h == height && self.back_buffer.is_some() {
            return Ok(());
        }

        self.back_buffer = None;
        self.shared_source = None;
        self.shared_source_handle = 0;
        unsafe {
            self.swap_chain
                .ResizeBuffers(
                    0,
                    width,
                    height,
                    DXGI_FORMAT_R8G8B8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
                .map_err(|e| format!("swapchain ResizeBuffers failed: {}", e))?;

            let back_buffer = self
                .swap_chain
                .GetBuffer::<ID3D11Texture2D>(0)
                .map_err(|e| format!("swapchain GetBuffer failed: {}", e))?;

            self.back_buffer = Some(back_buffer);
        }
        self.backbuffer_w = width;
        self.backbuffer_h = height;
        Ok(())
    }
}

impl Drop for NativeStreamPresenter {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.child_hwnd);
        }
    }
}
