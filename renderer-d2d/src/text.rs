//! Text rasterization tuning. Skia (mello's CPU renderer) and DirectWrite draw
//! the same glyph outlines at the same positions but weigh them differently:
//! DirectWrite applies the system's ClearType tuner gamma and an "enhanced
//! contrast" that thickens small text, and it renders a fractional baseline
//! as is where Skia snaps the baseline to a whole pixel. The lab's capture
//! comparison (WINDOWS.md 3.6, "Text tuning") found the snap alone halves the
//! difference to Skia, and every custom gamma and contrast setting came out
//! worse than the system's own ClearType tuner values. So the default is: snap
//! the baseline, keep the system parameters. Environment overrides for A/B runs:
//!
//! `FX_D2D_TEXT_SNAP_Y=0|1`; `FX_D2D_TEXT_PARAMS=1` switches to custom parameters
//! read from `FX_D2D_TEXT_GAMMA`, `FX_D2D_TEXT_CONTRAST`, `FX_D2D_TEXT_GRAY_CONTRAST`,
//! `FX_D2D_TEXT_MODE=default|natural|symmetric|outline`, `FX_D2D_TEXT_GRIDFIT=default|0|1`.

use std::sync::OnceLock;

use windows::core::Interface;
use windows::Win32::Graphics::Direct2D::ID2D1DeviceContext;
use windows::Win32::Graphics::DirectWrite::*;

pub struct TextTuning {
    /// Round every glyph's baseline to a whole pixel, as Skia does.
    pub snap_y: bool,
    /// Custom DirectWrite rendering parameters; `None` keeps the system's.
    pub params: Option<Params>,
}

pub struct Params {
    pub gamma: f32,
    pub contrast: f32,
    pub gray_contrast: f32,
    pub mode: DWRITE_RENDERING_MODE1,
    pub grid_fit: DWRITE_GRID_FIT_MODE,
}

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn tuning() -> &'static TextTuning {
    static TUNING: OnceLock<TextTuning> = OnceLock::new();
    TUNING.get_or_init(|| {
        let snap_y = std::env::var("FX_D2D_TEXT_SNAP_Y")
            .map(|v| v != "0")
            .unwrap_or(true);
        let custom = std::env::var("FX_D2D_TEXT_PARAMS").is_ok_and(|v| v == "1")
            || [
                "FX_D2D_TEXT_GAMMA",
                "FX_D2D_TEXT_CONTRAST",
                "FX_D2D_TEXT_GRAY_CONTRAST",
                "FX_D2D_TEXT_MODE",
                "FX_D2D_TEXT_GRIDFIT",
            ]
            .iter()
            .any(|n| std::env::var_os(n).is_some());
        let params = if !custom {
            None
        } else {
            let mode = match std::env::var("FX_D2D_TEXT_MODE").as_deref() {
                Ok("default") => DWRITE_RENDERING_MODE1_DEFAULT,
                Ok("natural") => DWRITE_RENDERING_MODE1_NATURAL,
                Ok("outline") => DWRITE_RENDERING_MODE1_OUTLINE,
                _ => DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC,
            };
            let grid_fit = match std::env::var("FX_D2D_TEXT_GRIDFIT").as_deref() {
                Ok("1") => DWRITE_GRID_FIT_MODE_ENABLED,
                Ok("default") => DWRITE_GRID_FIT_MODE_DEFAULT,
                _ => DWRITE_GRID_FIT_MODE_DISABLED,
            };
            Some(Params {
                gamma: env_f32("FX_D2D_TEXT_GAMMA", 2.2),
                contrast: env_f32("FX_D2D_TEXT_CONTRAST", 0.0),
                gray_contrast: env_f32("FX_D2D_TEXT_GRAY_CONTRAST", 0.0),
                mode,
                grid_fit,
            })
        };
        TextTuning { snap_y, params }
    })
}

/// Sets the rendering parameters on a device context. Failures keep the system's.
pub fn apply(dc: &ID2D1DeviceContext, dwrite: &IDWriteFactory5) {
    let Some(p) = tuning().params.as_ref() else {
        return;
    };
    unsafe {
        let Ok(factory3) = dwrite.cast::<IDWriteFactory3>() else {
            return;
        };
        if let Ok(params) = factory3.CreateCustomRenderingParams(
            p.gamma,
            p.contrast,
            p.gray_contrast,
            0.0,
            DWRITE_PIXEL_GEOMETRY_RGB,
            p.mode,
            p.grid_fit,
        ) {
            dc.SetTextRenderingParams(&params);
        }
    }
}
