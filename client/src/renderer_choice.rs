//! Which Slint renderer the Windows and Linux clients start with (spec
//! 12-NATIVE-PLATFORM section 3.1). Windows: `d2d` or `skia-software`. Linux:
//! Skia on wgpu (`skia-wgpu`, Vulkan) or `skia-software`, by the same rules,
//! with a Vulkan probe (platform/linux_gpu.rs).
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

// Windows and Linux pick a renderer here; the tests run everywhere.
#![cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]

/// mello's Direct2D renderer.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const GPU_RENDERER: &str = "d2d";
/// Linux: Skia on wgpu, Vulkan on the boot GPU.
pub const LINUX_GPU_RENDERER: &str = "skia-wgpu";
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
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn choose(
    override_name: Option<&str>,
    hardware_acceleration: bool,
    probe: impl FnOnce() -> Result<(), String>,
) -> (&'static str, Reason) {
    choose_gpu(GPU_RENDERER, override_name, hardware_acceleration, probe)
}

/// Linux: `choose` with Skia on wgpu as the GPU renderer. The probe checks
/// for a hardware Vulkan adapter; `skia-wgpu` makes its own device after it.
pub fn choose_linux(
    override_name: Option<&str>,
    hardware_acceleration: bool,
    probe: impl FnOnce() -> Result<(), String>,
) -> (&'static str, Reason) {
    choose_gpu(
        LINUX_GPU_RENDERER,
        override_name,
        hardware_acceleration,
        probe,
    )
}

fn choose_gpu(
    gpu_renderer: &'static str,
    override_name: Option<&str>,
    hardware_acceleration: bool,
    probe: impl FnOnce() -> Result<(), String>,
) -> (&'static str, Reason) {
    let (wants_gpu, reason) = match override_name {
        Some(name) if name == CPU_RENDERER => return (CPU_RENDERER, Reason::Override),
        Some(name) if name == gpu_renderer => (true, Reason::Override),
        _ if !hardware_acceleration => return (CPU_RENDERER, Reason::SettingOff),
        _ => (true, Reason::Default),
    };
    debug_assert!(wants_gpu);
    match probe() {
        Ok(()) => (gpu_renderer, reason),
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

    #[test]
    fn linux_defaults_to_skia_on_vulkan_when_the_probe_passes() {
        assert_eq!(
            choose_linux(None, true, ok),
            (LINUX_GPU_RENDERER, Reason::Default)
        );
    }

    #[test]
    fn linux_falls_back_without_a_vulkan_adapter() {
        let (name, reason) = choose_linux(None, true, || Err("no Vulkan adapter".into()));
        assert_eq!(name, CPU_RENDERER);
        assert_eq!(reason, Reason::ProbeFailed("no Vulkan adapter".into()));
    }

    #[test]
    fn linux_keeps_the_setting_and_the_override_rules() {
        let probed = Cell::new(false);
        let probe = || {
            probed.set(true);
            Ok(())
        };
        assert_eq!(
            choose_linux(None, false, probe),
            (CPU_RENDERER, Reason::SettingOff)
        );
        assert!(!probed.get(), "the setting must not touch Vulkan");
        assert_eq!(
            choose_linux(Some(LINUX_GPU_RENDERER), false, ok),
            (LINUX_GPU_RENDERER, Reason::Override)
        );
        assert_eq!(
            choose_linux(Some(CPU_RENDERER), true, ok),
            (CPU_RENDERER, Reason::Override)
        );
        // The Windows renderer is unknown here and ignored.
        assert_eq!(
            choose_linux(Some(GPU_RENDERER), true, ok),
            (LINUX_GPU_RENDERER, Reason::Default)
        );
    }
}
