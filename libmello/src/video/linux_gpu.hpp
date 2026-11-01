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
/// - Else the decoder of the primary GPU goes first, so a laptop with two
///   GPUs keeps its NVIDIA GPU asleep: NVDEC first when the primary GPU is
///   NVIDIA, VA-API first otherwise. OpenH264 is always last.
std::vector<std::string> linux_decoder_order(const std::vector<LinuxGpu>& gpus, bool soft,
                                             const std::string& forced);

/// The GPU that encodes a stream, as on Windows (graphics_device.cpp: the
/// adapter with the most dedicated video memory): an NVIDIA GPU when there
/// is one, else the GPU with the most video memory, else the primary GPU.
/// On a laptop that is the discrete GPU the game runs on. Viewing keeps the
/// primary GPU (linux_decoder_order). Null without GPUs.
const LinuxGpu* encode_gpu(const std::vector<LinuxGpu>& gpus);

/// The encoders to try, in order: "VA-API", "NVENC", or "OpenH264" alone.
///
/// - `soft`: OpenH264 only (MELLO_SOFT_ENCODER=1, the test lane as on
///   Windows). Hardware encode only otherwise (spec 03 section 5).
/// - `forced`: MELLO_LINUX_ENCODER (va, nvenc or openh264) goes first;
///   openh264 means OpenH264 only.
/// - Else the encode GPU's encoder first (encode_gpu): NVENC for NVIDIA.
std::vector<std::string> linux_encoder_order(const std::vector<LinuxGpu>& gpus, bool soft,
                                             const std::string& forced);

/// The render node of encode_gpu(). Empty if unknown.
std::string encode_render_node(const std::vector<LinuxGpu>& gpus);

/// The render node of the primary GPU, or of the only GPU. Empty if unknown.
std::string primary_render_node(const std::vector<LinuxGpu>& gpus);

/// The VA driver of the primary GPU on a machine with two or more GPUs whose
/// primary GPU is AMD or Intel: "radeonsi", "iHD" or "i965" (the first whose
/// `<name>_drv_video.so` is in `driver_dirs`). Empty otherwise.
///
/// GStreamer's VA plugin opens a VA display on every render node when it
/// loads. On an NVIDIA node with nvidia-vaapi-driver installed that starts
/// CUDA and wakes the NVIDIA GPU, which the decoder order keeps asleep.
/// LIBVA_DRIVER_NAME set to this driver makes the NVIDIA node fail at once.
std::string va_driver_for_primary_gpu(const std::vector<LinuxGpu>& gpus,
                                      const std::vector<std::string>& driver_dirs);

/// The VA driver directories: LIBVA_DRIVERS_PATH, else the distribution
/// defaults.
std::vector<std::string> va_driver_dirs();

/// Sets LIBVA_DRIVER_NAME to va_driver_for_primary_gpu() when it is unset,
/// and GST_GL_WINDOW=surfaceless (GStreamer's GL for NVENC runs without a
/// window) when that is unset. Once per process, before GStreamer loads.
/// Call it on the thread that starts libmello: setenv must not race another
/// thread's getenv.
void prepare_va_driver_env();

} // namespace mello::video
#endif
