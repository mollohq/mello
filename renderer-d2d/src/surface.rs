//! The DXGI swapchain on one window and the Direct2D device context that draws
//! on its back buffer.
//!
//! Frame: `begin_frame` wraps the back buffer in a target bitmap and opens a
//! draw, `end_frame` closes it, drops the target and presents with vsync.
//! Resize: `ensure_size` drops the target and resizes the buffers. The shape
//! is `win\driver-cost\src\d3d11.rs` (`d2d` mode), which measured at 14 MB
//! private working set.

use std::rc::Rc;

use i_slint_core::platform::PlatformError;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use crate::device::{err, SharedDevice};

pub struct Surface {
    pub device: Rc<SharedDevice>,
    swapchain: IDXGISwapChain1,
    pub dc: ID2D1DeviceContext,
    width: u32,
    height: u32,
}

fn target_properties(options: D2D1_BITMAP_OPTIONS) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: options,
        colorContext: std::mem::ManuallyDrop::new(None),
    }
}

impl Surface {
    pub fn new(hwnd: isize, width: u32, height: u32) -> Result<Self, PlatformError> {
        let device = SharedDevice::get()?;
        let width = width.max(1);
        let height = height.max(1);
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
                .map_err(|e| err("CreateDXGIFactory2", e))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let swapchain = factory
                .CreateSwapChainForHwnd(&device.d3d, HWND(hwnd as *mut _), &desc, None, None)
                .map_err(|e| err("CreateSwapChainForHwnd", e))?;
            let dc = device
                .d2d
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                .map_err(|e| err("CreateDeviceContext", e))?;
            // One pixel per DIP: Slint hands the renderer physical pixels.
            dc.SetDpi(96.0, 96.0);
            dc.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
            // Grayscale, as Skia renders; ClearType needs an opaque target it can read back.
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            crate::text::apply(&dc, &device.dwrite);
            Ok(Self {
                device,
                swapchain,
                dc,
                width,
                height,
            })
        }
    }

    pub fn ensure_size(&mut self, width: u32, height: u32) -> Result<(), PlatformError> {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            return Ok(());
        }
        unsafe {
            self.dc.SetTarget(None);
            self.swapchain
                .ResizeBuffers(
                    0,
                    width,
                    height,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
                .map_err(|e| err("ResizeBuffers", e))?;
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Wraps the back buffer in a Direct2D target and opens the draw. The returned
    /// bitmap must live until `end_frame`.
    pub fn begin_frame(&self) -> Result<ID2D1Bitmap1, PlatformError> {
        unsafe {
            let back: IDXGISurface = self
                .swapchain
                .GetBuffer(0)
                .map_err(|e| err("GetBuffer", e))?;
            let props =
                target_properties(D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW);
            let target = self
                .dc
                .CreateBitmapFromDxgiSurface(&back, Some(&props))
                .map_err(|e| err("CreateBitmapFromDxgiSurface", e))?;
            self.dc.SetTarget(&target);
            self.dc.BeginDraw();
            Ok(target)
        }
    }

    /// Closes the draw and presents. With `read_back`, copies the frame to the CPU
    /// first: premultiplied BGRA, `width * 4` bytes per row. Returns the time
    /// `EndDraw` took (Direct2D's tessellation and command submission) and the time
    /// the present call took (the vsync wait).
    pub fn end_frame(
        &self,
        target: ID2D1Bitmap1,
        pre_present: &mut Option<Box<dyn FnMut()>>,
        read_back: Option<&mut Vec<u8>>,
    ) -> Result<(std::time::Duration, std::time::Duration), windows::core::Error> {
        unsafe {
            let t = std::time::Instant::now();
            let drawn = self.dc.EndDraw(None, None);
            let flush = t.elapsed();
            if drawn.is_ok() {
                if let Some(out) = read_back {
                    self.read_back(&target, out)?;
                }
            }
            self.dc.SetTarget(None);
            drop(target);
            drawn?;
            if let Some(cb) = pre_present.as_mut() {
                cb();
            }
            let t = std::time::Instant::now();
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
            Ok((flush, t.elapsed()))
        }
    }

    unsafe fn read_back(
        &self,
        target: &ID2D1Bitmap1,
        out: &mut Vec<u8>,
    ) -> Result<(), windows::core::Error> {
        unsafe {
            let props =
                target_properties(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW);
            let staging = self.dc.CreateBitmap(
                D2D_SIZE_U {
                    width: self.width,
                    height: self.height,
                },
                None,
                0,
                &props,
            )?;
            staging.CopyFromBitmap(None, target, None)?;
            let mapped = staging.Map(D2D1_MAP_OPTIONS_READ)?;
            let row = (self.width * 4) as usize;
            out.clear();
            out.reserve(row * self.height as usize);
            for y in 0..self.height as usize {
                let src =
                    std::slice::from_raw_parts(mapped.bits.add(y * mapped.pitch as usize), row);
                out.extend_from_slice(src);
            }
            staging.Unmap()?;
        }
        Ok(())
    }
}
