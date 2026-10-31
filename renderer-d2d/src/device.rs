//! One D3D11 device, one Direct2D device and one DirectWrite factory per thread,
//! shared by every window's surface (D2D-RENDERER.md 3.1). Dropped as a whole
//! when the device is removed; the next surface recreates it.

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_core::platform::PlatformError;
use windows::core::Interface;
use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HMODULE};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::*;

pub struct SharedDevice {
    pub d3d: ID3D11Device,
    /// Kept so the immediate context lives as long as the device.
    _d3d_context: ID3D11DeviceContext,
    pub d2d_factory: ID2D1Factory1,
    pub d2d: ID2D1Device,
    /// A second context for offscreen work (shadow bitmaps), so the window's
    /// context never changes target inside a frame.
    pub offscreen: ID2D1DeviceContext,
    pub dwrite: IDWriteFactory5,
}

thread_local! {
    static DEVICE: RefCell<Option<Rc<SharedDevice>>> = const { RefCell::new(None) };
}

/// 24 MB of Direct2D-managed textures before it purges.
const TEXTURE_BUDGET: u64 = 24 << 20;

pub fn err(what: &str, e: windows::core::Error) -> PlatformError {
    format!("d2d renderer: {what}: {e}").into()
}

/// True for the errors after which the device and everything made from it must go.
pub fn is_device_lost(e: &windows::core::Error) -> bool {
    let code = e.code();
    code == DXGI_ERROR_DEVICE_REMOVED
        || code == DXGI_ERROR_DEVICE_RESET
        || code == D2DERR_RECREATE_TARGET
}

impl SharedDevice {
    fn create() -> Result<Self, PlatformError> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| err("D3D11CreateDevice", e))?;
            let d3d = device.ok_or_else(|| PlatformError::from("d2d renderer: no device"))?;
            let d3d_context =
                context.ok_or_else(|| PlatformError::from("d2d renderer: no context"))?;
            let dxgi: IDXGIDevice = d3d.cast().map_err(|e| err("IDXGIDevice", e))?;
            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                    .map_err(|e| err("D2D1CreateFactory", e))?;
            let d2d = d2d_factory
                .CreateDevice(&dxgi)
                .map_err(|e| err("ID2D1Factory1::CreateDevice", e))?;
            // Direct2D purges its own caches (glyph atlases, layer and effect
            // intermediates) past this much texture memory (memory rule 2).
            d2d.SetMaximumTextureMemory(TEXTURE_BUDGET);
            let offscreen = d2d
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                .map_err(|e| err("CreateDeviceContext", e))?;
            offscreen.SetDpi(96.0, 96.0);
            offscreen.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
            let dwrite: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                .map_err(|e| err("DWriteCreateFactory", e))?;
            Ok(Self {
                d3d,
                _d3d_context: d3d_context,
                d2d_factory,
                d2d,
                offscreen,
                dwrite,
            })
        }
    }

    /// The thread's device, created on first use or after `reset`.
    pub fn get() -> Result<Rc<SharedDevice>, PlatformError> {
        DEVICE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(d) = slot.as_ref() {
                return Ok(d.clone());
            }
            let d = Rc::new(Self::create()?);
            *slot = Some(d.clone());
            Ok(d)
        })
    }

    /// Forgets the thread's device. Surfaces that still hold an `Rc` keep it alive
    /// until they are dropped; new surfaces get a new device.
    pub fn reset() {
        DEVICE.with(|slot| *slot.borrow_mut() = None);
    }

    /// The thread's device if one exists; never creates one.
    pub fn try_get() -> Option<Rc<SharedDevice>> {
        DEVICE.with(|slot| slot.borrow().clone())
    }

    /// Drops the device when no surface uses it any more (every window suspended),
    /// so a hidden app holds no D3D11 or Direct2D memory at all.
    pub fn release_if_unused() {
        DEVICE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.as_ref().is_some_and(|d| Rc::strong_count(d) == 1) {
                *slot = None;
            }
        });
    }

    /// Asks Direct2D to drop every cached resource unused for `since_ms`.
    pub fn trim(&self, since_ms: u32) {
        unsafe { self.d2d.ClearResources(since_ms) };
    }
}
