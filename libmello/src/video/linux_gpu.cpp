#if defined(__linux__)
#include "linux_gpu.hpp"

#include <algorithm>
#include <climits>
#include <cstdlib>
#include <dirent.h>
#include <fstream>
#include <map>

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

} // namespace mello::video
#endif
