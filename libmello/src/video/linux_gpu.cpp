#if defined(__linux__)
#include "linux_gpu.hpp"

#include <algorithm>
#include <climits>
#include <cstdlib>
#include <dirent.h>
#include <fstream>
#include <map>
#include <mutex>
#include <sstream>
#include <sys/stat.h>

#include "../util/log.hpp"

namespace mello::video {

static std::string read_line(const std::string& path) {
    std::ifstream in(path);
    std::string line;
    std::getline(in, line);
    return line;
}

static std::string real_path(const std::string& path) {
    char buf[PATH_MAX];
    return realpath(path.c_str(), buf) ? std::string(buf) : std::string{};
}

std::vector<LinuxGpu> list_linux_gpus(const std::string& drm_root) {
    std::vector<std::string> cards, renders;
    if (DIR* dir = opendir(drm_root.c_str())) {
        while (dirent* e = readdir(dir)) {
            const std::string name = e->d_name;
            // card0, renderD128; not card0-eDP-1 (a connector).
            if (name.rfind("card", 0) == 0 && name.find('-') == std::string::npos) cards.push_back(name);
            if (name.rfind("renderD", 0) == 0) renders.push_back(name);
        }
        closedir(dir);
    }
    std::sort(cards.begin(), cards.end());

    // Render node per PCI device.
    std::map<std::string, std::string> render_of;
    for (const std::string& r : renders) {
        const std::string dev = real_path(drm_root + "/" + r + "/device");
        if (!dev.empty()) render_of[dev] = "/dev/dri/" + r;
    }

    std::vector<LinuxGpu> gpus;
    for (const std::string& c : cards) {
        const std::string base = drm_root + "/" + c + "/device";
        const std::string vendor = read_line(base + "/vendor");
        if (vendor.empty()) continue;
        LinuxGpu gpu;
        gpu.vendor = static_cast<unsigned>(std::strtoul(vendor.c_str(), nullptr, 16));
        gpu.primary = read_line(base + "/boot_vga") == "1";
        const auto it = render_of.find(real_path(base));
        if (it != render_of.end()) gpu.render_node = it->second;
        gpus.push_back(gpu);
    }
    return gpus;
}

std::vector<std::string> linux_decoder_order(const std::vector<LinuxGpu>& gpus, bool soft,
                                             const std::string& forced) {
    if (soft) return {"OpenH264"};
    const LinuxGpu* primary = nullptr;
    for (const LinuxGpu& g : gpus) {
        if (g.primary) primary = &g;
    }
    if (!primary && gpus.size() == 1) primary = &gpus[0];

    std::vector<std::string> order;
    if (primary && primary->vendor == kVendorNvidia) {
        order = {"NVDEC", "VA-API", "OpenH264"};
    } else {
        order = {"VA-API", "NVDEC", "OpenH264"};
    }
    std::string first;
    if (forced == "nvdec") first = "NVDEC";
    if (forced == "va") first = "VA-API";
    if (forced == "openh264") first = "OpenH264";
    if (!first.empty()) {
        order.erase(std::find(order.begin(), order.end(), first));
        order.insert(order.begin(), first);
    }
    return order;
}

std::string primary_render_node(const std::vector<LinuxGpu>& gpus) {
    for (const LinuxGpu& g : gpus) {
        if (g.primary) return g.render_node;
    }
    return gpus.size() == 1 ? gpus[0].render_node : std::string{};
}

static bool file_exists(const std::string& path) {
    struct stat st{};
    return stat(path.c_str(), &st) == 0;
}

std::string va_driver_for_primary_gpu(const std::vector<LinuxGpu>& gpus,
                                      const std::vector<std::string>& driver_dirs) {
    if (gpus.size() < 2) return {};
    const LinuxGpu* primary = nullptr;
    for (const LinuxGpu& g : gpus) {
        if (g.primary) primary = &g;
    }
    if (!primary) return {};
    std::vector<std::string> candidates;
    if (primary->vendor == kVendorAmd) candidates = {"radeonsi"};
    if (primary->vendor == kVendorIntel) candidates = {"iHD", "i965"};
    for (const std::string& name : candidates) {
        for (const std::string& dir : driver_dirs) {
            if (file_exists(dir + "/" + name + "_drv_video.so")) return name;
        }
    }
    return {};
}

std::vector<std::string> va_driver_dirs() {
    if (const char* env = std::getenv("LIBVA_DRIVERS_PATH")) {
        std::vector<std::string> dirs;
        std::stringstream in(env);
        std::string dir;
        while (std::getline(in, dir, ':')) {
            if (!dir.empty()) dirs.push_back(dir);
        }
        return dirs;
    }
    return {"/usr/lib/dri", "/usr/lib64/dri", "/usr/lib/x86_64-linux-gnu/dri"};
}

void prepare_va_driver_env() {
    static std::once_flag once;
    std::call_once(once, [] {
        if (std::getenv("LIBVA_DRIVER_NAME")) return;
        const std::string driver = va_driver_for_primary_gpu(list_linux_gpus(), va_driver_dirs());
        if (driver.empty()) return;
        setenv("LIBVA_DRIVER_NAME", driver.c_str(), 0);
        MELLO_LOG_INFO("video/decoder", "VA-API: LIBVA_DRIVER_NAME=%s (the primary GPU's driver, so "
                                        "GStreamer leaves the other GPU asleep)", driver.c_str());
    });
}

} // namespace mello::video
#endif
