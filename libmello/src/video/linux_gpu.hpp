#pragma once
#if defined(__linux__)
#include <string>
#include <vector>

namespace mello::video {

/// One GPU of the machine, from /sys/class/drm.
struct LinuxGpu {
    /// PCI vendor id: 0x10de NVIDIA, 0x1002 AMD, 0x8086 Intel.
    unsigned vendor = 0;
    /// The firmware's boot GPU (`boot_vga`): the one that draws the desktop
    /// on a laptop with two GPUs.
    bool primary = false;
    /// Its render node, `/dev/dri/renderD128`. Empty when it has none.
    std::string render_node;
    /// Its primary node, `/dev/dri/card0`.
    std::string card_node;
    /// amdgpu's `mem_info_vram_total`: dedicated video memory (an APU's is a
    /// small carve-out). 0 when the driver does not tell (NVIDIA, Intel).
    uint64_t vram_bytes = 0;
    /// Its PCI address, `0000:01:00.0`.
    std::string pci_address;
};

constexpr unsigned kVendorNvidia = 0x10de;
constexpr unsigned kVendorAmd = 0x1002;
constexpr unsigned kVendorIntel = 0x8086;

/// The GPUs under `drm_root` (a test passes a fake tree).
std::vector<LinuxGpu> list_linux_gpus(const std::string& drm_root = "/sys/class/drm");

/// The decoders to try, in order: "NVDEC", "VA-API", "OpenH264".
///
/// - `soft`: OpenH264 only (MELLO_SOFT_DECODER=1).
/// - `forced`: MELLO_LINUX_DECODER (nvdec, va or openh264) goes first.
/// - Else the video GPU's decoder goes first (video_gpu): NVDEC for NVIDIA,
///   VA-API otherwise. OpenH264 is always last.
std::vector<std::string> linux_decoder_order(const std::vector<LinuxGpu>& gpus, bool soft,
                                             const std::string& forced);

/// The GPU for all video work, capture to encode and decode, as on Windows
/// (graphics_device.cpp: the adapter with the most dedicated video memory,
/// whatever the brand): on a laptop the discrete GPU. Dedicated memory is
/// amdgpu's mem_info_vram_total (an APU shows its small carve-out); NVIDIA
/// tells sysfs nothing and counts as 2 GiB, more than any carve-out, or its
/// NVML total when another GPU has 2 GiB or more. Ties go to the primary
/// GPU. Only the UI's Vulkan stays on the primary GPU. Null without GPUs.
const LinuxGpu* video_gpu(const std::vector<LinuxGpu>& gpus);

/// The encoders to try, in order: "VA-API", "NVENC", or "OpenH264" alone.
///
/// - `soft`: OpenH264 only (MELLO_SOFT_ENCODER=1, the test lane as on
///   Windows). Hardware encode only otherwise (spec 03 section 5).
/// - `forced`: MELLO_LINUX_ENCODER (va, nvenc or openh264) goes first;
///   openh264 means OpenH264 only.
/// - Else the video GPU's encoder first (video_gpu): NVENC for NVIDIA.
std::vector<std::string> linux_encoder_order(const std::vector<LinuxGpu>& gpus, bool soft,
                                             const std::string& forced);

/// The render node of video_gpu(). Empty if unknown.
std::string video_render_node(const std::vector<LinuxGpu>& gpus);

/// The render node of the primary GPU, or of the only GPU. Empty if unknown.
std::string primary_render_node(const std::vector<LinuxGpu>& gpus);

/// The VA driver of the video GPU on a machine with two or more GPUs whose
/// video GPU is AMD or Intel: "radeonsi", "iHD" or "i965" (the first whose
/// `<name>_drv_video.so` is in `driver_dirs`). Empty otherwise.
///
/// GStreamer's VA plugin opens a VA display on every render node when it
/// loads. On an NVIDIA node with nvidia-vaapi-driver installed that starts
/// CUDA in our process for nothing. LIBVA_DRIVER_NAME set to this driver
/// makes the other GPU's node fail at once.
std::string va_driver_for_video_gpu(const std::vector<LinuxGpu>& gpus,
                                    const std::vector<std::string>& driver_dirs);

/// The VA driver directories: LIBVA_DRIVERS_PATH, else the distribution
/// defaults.
std::vector<std::string> va_driver_dirs();

/// Sets LIBVA_DRIVER_NAME to va_driver_for_video_gpu() when it is unset,
/// and GST_GL_WINDOW=surfaceless (GStreamer's GL for NVENC runs without a
/// window) when that is unset. Once per process, before GStreamer loads.
/// Call it on the thread that starts libmello: setenv must not race another
/// thread's getenv.
void prepare_va_driver_env();

} // namespace mello::video
#endif
