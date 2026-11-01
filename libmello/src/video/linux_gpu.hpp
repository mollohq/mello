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

/// The render node of the primary GPU, or of the only GPU. Empty if unknown.
std::string primary_render_node(const std::vector<LinuxGpu>& gpus);

} // namespace mello::video
#endif
