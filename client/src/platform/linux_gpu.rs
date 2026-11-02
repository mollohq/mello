//! The GPU side of the Linux renderer choice (renderer_choice.rs).
//!
//! Two jobs, both done before Slint starts:
//! - The Vulkan loader opens every installed driver when wgpu enumerates
//!   adapters. On a laptop with two GPUs that loads NVIDIA's driver too (20
//!   handles and about 40 MB of private memory on the test laptop), next to
//!   the GPU that draws the desktop. `prepare_vulkan_driver_env` limits the
//!   loader to the driver of the boot GPU (`boot_vga` in /sys/class/drm).
//! - `probe_vulkan` checks that wgpu finds a hardware Vulkan adapter, so the
//!   client falls back to Skia's CPU rasterizer where `skia-wgpu` cannot run.

use std::path::Path;

const VENDOR_NVIDIA: u32 = 0x10de;
const VENDOR_AMD: u32 = 0x1002;
const VENDOR_INTEL: u32 = 0x8086;

/// One GPU from /sys/class/drm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpu {
    pub vendor: u32,
    /// The firmware's boot GPU, the one that draws the desktop.
    pub primary: bool,
}

/// The GPUs under `drm_root` (`card<N>` entries; connectors like
/// `card1-eDP-1` are not GPUs).
pub fn list_gpus(drm_root: &Path) -> Vec<Gpu> {
    let Ok(entries) = std::fs::read_dir(drm_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("card") && !n.contains('-'))
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|card| {
            let dev = drm_root.join(card).join("device");
            let vendor = std::fs::read_to_string(dev.join("vendor")).ok()?;
            let vendor = u32::from_str_radix(vendor.trim().trim_start_matches("0x"), 16).ok()?;
            let primary = std::fs::read_to_string(dev.join("boot_vga"))
                .map(|s| s.trim() == "1")
                .unwrap_or(false);
            Some(Gpu { vendor, primary })
        })
        .collect()
}

/// The Vulkan loader's driver filter for the boot GPU on a machine with two
/// or more GPUs: globs over the driver manifest names (radeon_icd.json for
/// RADV, amd_icd64.json for AMDVLK, intel_icd.json, nvidia_icd.json). None
/// with one GPU or no boot GPU: the loader then needs no help.
pub fn vulkan_driver_select(gpus: &[Gpu]) -> Option<&'static str> {
    if gpus.len() < 2 {
        return None;
    }
    match gpus.iter().find(|g| g.primary)?.vendor {
        VENDOR_AMD => Some("*radeon*,*amd*"),
        VENDOR_INTEL => Some("*intel*"),
        VENDOR_NVIDIA => Some("*nvidia*"),
        _ => None,
    }
}

/// Sets VK_LOADER_DRIVERS_SELECT for the boot GPU unless the user chose the
/// Vulkan drivers already. Called once, early in `configure_slint_platform`,
/// before libmello and the audio threads start. A loader older than 1.3.234
/// ignores the variable and opens every driver, as before.
pub fn prepare_vulkan_driver_env() {
    const USER_CHOICE: [&str; 3] = [
        "VK_LOADER_DRIVERS_SELECT",
        "VK_DRIVER_FILES",
        "VK_ICD_FILENAMES",
    ];
    if USER_CHOICE.iter().any(|v| std::env::var_os(v).is_some()) {
        return;
    }
    if let Some(select) = vulkan_driver_select(&list_gpus(Path::new("/sys/class/drm"))) {
        std::env::set_var("VK_LOADER_DRIVERS_SELECT", select);
        log::info!(
            "[startup] VK_LOADER_DRIVERS_SELECT={select} (the boot GPU's Vulkan driver only)"
        );
    }
}

/// Ok when wgpu, as Slint's Skia renderer uses it, finds a hardware Vulkan
/// adapter. A software adapter (llvmpipe) is an error: Skia's CPU rasterizer
/// is faster than Skia on a software Vulkan.
pub fn probe_vulkan() -> Result<(), String> {
    use futures_util::FutureExt;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    // Native wgpu enumerates synchronously: the future is ready at once.
    let adapters = instance
        .enumerate_adapters(wgpu::Backends::VULKAN)
        .now_or_never()
        .ok_or("wgpu adapter enumeration did not complete")?;
    let found: Vec<(String, wgpu::DeviceType)> = adapters
        .iter()
        .map(|a| {
            let info = a.get_info();
            (info.name, info.device_type)
        })
        .collect();
    let (name, device_type) = pick_hardware_adapter(&found)?;
    log::info!("[startup] Vulkan adapter: {name} ({device_type:?})");
    Ok(())
}

type DeviceType = wgpu::DeviceType;

fn pick_hardware_adapter(found: &[(String, DeviceType)]) -> Result<&(String, DeviceType), String> {
    let first = found.first().ok_or("no Vulkan adapter")?;
    found
        .iter()
        .find(|(_, t)| *t != DeviceType::Cpu)
        .ok_or_else(|| format!("only a software Vulkan adapter ({})", first.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fake_drm(cards: &[(&str, &str, Option<&str>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for (card, vendor, boot_vga) in cards {
            let dev = dir.path().join(card).join("device");
            fs::create_dir_all(&dev).expect("device dir");
            fs::write(dev.join("vendor"), format!("{vendor}\n")).expect("vendor");
            if let Some(b) = boot_vga {
                fs::write(dev.join("boot_vga"), format!("{b}\n")).expect("boot_vga");
            }
        }
        dir
    }

    #[test]
    fn a_hybrid_laptop_lists_both_gpus_and_its_boot_gpu() {
        let dir = fake_drm(&[
            ("card0", "0x10de", Some("0")),
            ("card1", "0x1002", Some("1")),
        ]);
        fs::create_dir_all(dir.path().join("card1-eDP-1")).expect("connector");
        assert_eq!(
            list_gpus(dir.path()),
            vec![
                Gpu {
                    vendor: VENDOR_NVIDIA,
                    primary: false
                },
                Gpu {
                    vendor: VENDOR_AMD,
                    primary: true
                },
            ]
        );
    }

    #[test]
    fn the_filter_names_the_boot_gpus_driver_only_with_two_gpus() {
        let amd = Gpu {
            vendor: VENDOR_AMD,
            primary: true,
        };
        let nvidia = Gpu {
            vendor: VENDOR_NVIDIA,
            primary: false,
        };
        assert_eq!(
            vulkan_driver_select(&[nvidia.clone(), amd.clone()]),
            Some("*radeon*,*amd*")
        );
        assert_eq!(vulkan_driver_select(std::slice::from_ref(&amd)), None);
        let intel = Gpu {
            vendor: VENDOR_INTEL,
            primary: true,
        };
        assert_eq!(
            vulkan_driver_select(&[intel, nvidia.clone()]),
            Some("*intel*")
        );
        let boot_nvidia = Gpu {
            vendor: VENDOR_NVIDIA,
            primary: true,
        };
        let other = Gpu {
            vendor: VENDOR_AMD,
            primary: false,
        };
        assert_eq!(
            vulkan_driver_select(&[boot_nvidia, other]),
            Some("*nvidia*")
        );
        // No boot GPU marked: no filter.
        let a = Gpu {
            vendor: VENDOR_AMD,
            primary: false,
        };
        assert_eq!(vulkan_driver_select(&[a, nvidia]), None);
    }

    fn adapter(name: &str, t: DeviceType) -> (String, DeviceType) {
        (name.to_string(), t)
    }

    #[test]
    fn a_software_vulkan_adapter_is_not_enough() {
        assert!(pick_hardware_adapter(&[]).is_err());
        let err =
            pick_hardware_adapter(&[adapter("llvmpipe", DeviceType::Cpu)]).expect_err("software");
        assert!(err.contains("llvmpipe"), "{err}");
        let found = [
            adapter("llvmpipe", DeviceType::Cpu),
            adapter("AMD Radeon", DeviceType::IntegratedGpu),
        ];
        assert_eq!(
            pick_hardware_adapter(&found).map(|(n, _)| n.as_str()),
            Ok("AMD Radeon")
        );
    }
}
