//! Which Slint renderer the Windows client starts with (spec 12-NATIVE-PLATFORM
//! section 3.1).
//!
//! The GPU renderer is mello's own Direct2D renderer (`renderer-d2d`, name
//! `d2d`). It needs a D3D11 device; where one cannot be made (no GPU driver, a
//! remote desktop session, a driver that is being updated) the client falls
//! back to Skia's CPU rasterizer (`skia-software`), the renderer every Windows
//! build before it used. The "Hardware Acceleration" setting turns the GPU
//! renderer off; `MELLO_RENDERER` overrides both for debugging.
//!
//! The decision is a pure function of its inputs so it can be tested without a
//! GPU: the device probe is passed in.

// Only Windows picks a renderer here; the tests run everywhere.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// mello's Direct2D renderer.
pub const GPU_RENDERER: &str = "d2d";
/// Skia's CPU rasterizer, the fallback.
pub const CPU_RENDERER: &str = "skia-software";

/// Why a renderer was chosen. Logged at startup.
#[derive(Debug, PartialEq, Eq)]
pub enum Reason {
    /// `MELLO_RENDERER` named it.
    Override,
    /// The "Hardware Acceleration" setting is off.
    SettingOff,
    /// The GPU device could not be created; the message says why.
    ProbeFailed(String),
    /// The default.
    Default,
}

/// Picks the renderer name for the winit backend.
///
/// - `override_name`: the `MELLO_RENDERER` value, if set. Only the two renderer
///   names are accepted; anything else is ignored.
/// - `hardware_acceleration`: the setting. Off means the CPU renderer, and the
///   probe does not run.
/// - `probe`: creates the GPU device once. It runs only when the GPU renderer is
///   about to be chosen; `Err` turns the choice into the CPU renderer. The device
///   it creates is the one the renderer then uses, so the probe is not wasted.
pub fn choose(
    override_name: Option<&str>,
    hardware_acceleration: bool,
    probe: impl FnOnce() -> Result<(), String>,
) -> (&'static str, Reason) {
    let (wants_gpu, reason) = match override_name {
        Some(name) if name == CPU_RENDERER => return (CPU_RENDERER, Reason::Override),
        Some(name) if name == GPU_RENDERER => (true, Reason::Override),
        _ if !hardware_acceleration => return (CPU_RENDERER, Reason::SettingOff),
        _ => (true, Reason::Default),
    };
    debug_assert!(wants_gpu);
    match probe() {
        Ok(()) => (GPU_RENDERER, reason),
        Err(why) => (CPU_RENDERER, Reason::ProbeFailed(why)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn ok() -> Result<(), String> {
        Ok(())
    }

    fn fails() -> Result<(), String> {
        Err("D3D11CreateDevice: no device".into())
    }

    #[test]
    fn default_is_the_gpu_renderer_when_the_device_probe_passes() {
        assert_eq!(choose(None, true, ok), (GPU_RENDERER, Reason::Default));
    }

    #[test]
    fn a_failed_probe_falls_back_to_the_cpu_renderer() {
        let (name, reason) = choose(None, true, fails);
        assert_eq!(name, CPU_RENDERER);
        assert_eq!(
            reason,
            Reason::ProbeFailed("D3D11CreateDevice: no device".into())
        );
    }

    #[test]
    fn hardware_acceleration_off_means_the_cpu_renderer_without_a_probe() {
        let probed = Cell::new(false);
        let (name, reason) = choose(None, false, || {
            probed.set(true);
            Ok(())
        });
        assert_eq!((name, reason), (CPU_RENDERER, Reason::SettingOff));
        assert!(!probed.get(), "the setting must not create a GPU device");
    }

    #[test]
    fn the_override_can_force_the_cpu_renderer_without_a_probe() {
        let probed = Cell::new(false);
        let (name, reason) = choose(Some(CPU_RENDERER), true, || {
            probed.set(true);
            Ok(())
        });
        assert_eq!((name, reason), (CPU_RENDERER, Reason::Override));
        assert!(!probed.get());
    }

    #[test]
    fn the_override_can_force_the_gpu_renderer_past_the_setting() {
        assert_eq!(
            choose(Some(GPU_RENDERER), false, ok),
            (GPU_RENDERER, Reason::Override)
        );
    }

    #[test]
    fn the_override_still_falls_back_when_the_device_cannot_be_made() {
        let (name, reason) = choose(Some(GPU_RENDERER), true, fails);
        assert_eq!(name, CPU_RENDERER);
        assert!(matches!(reason, Reason::ProbeFailed(_)));
    }

    #[test]
    fn an_unknown_override_is_ignored() {
        assert_eq!(
            choose(Some("skia-opengl"), true, ok),
            (GPU_RENDERER, Reason::Default)
        );
        assert_eq!(
            choose(Some("femtovg"), false, ok),
            (CPU_RENDERER, Reason::SettingOff)
        );
    }
}
