#if defined(__linux__)
#include "linux_gpu.hpp"

#include <algorithm>
#include <climits>
#include <cstdlib>
#include <dirent.h>
#include <dlfcn.h>
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

// NVIDIA has no integrated GPUs on PCs: with its memory unknown it counts
// as this much, more than any integrated GPU's carve-out.
static constexpr uint64_t kAssumedNvidiaVram = 2ull << 30;

// Total memory of the NVIDIA GPU at `pci_address` from NVML, the NVIDIA
// driver's management library; 0 when it does not answer.
static uint64_t nvidia_vram(const std::string& pci_address) {
    struct NvmlMemory { unsigned long long total, free, used; };
    void* nvml = dlopen("libnvidia-ml.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!nvml) return 0;
    auto init = reinterpret_cast<int (*)()>(dlsym(nvml, "nvmlInit_v2"));
    auto by_pci = reinterpret_cast<int (*)(const char*, void**)>(dlsym(nvml, "nvmlDeviceGetHandleByPciBusId_v2"));
    auto memory = reinterpret_cast<int (*)(void*, NvmlMemory*)>(dlsym(nvml, "nvmlDeviceGetMemoryInfo"));
    auto shutdown = reinterpret_cast<int (*)()>(dlsym(nvml, "nvmlShutdown"));
    uint64_t total = 0;
    if (init && by_pci && memory && shutdown && init() == 0) {
        void* device = nullptr;
        NvmlMemory m{};
        if (by_pci(pci_address.c_str(), &device) == 0 && memory(device, &m) == 0) total = m.total;
        shutdown();
    }
    dlclose(nvml);
    return total;
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
        gpu.card_node = "/dev/dri/" + c;
        gpu.vram_bytes = std::strtoull(read_line(base + "/mem_info_vram_total").c_str(), nullptr, 10);
        const auto it = render_of.find(real_path(base));
        if (it != render_of.end()) gpu.render_node = it->second;
        gpu.pci_address = real_path(base);
        gpu.pci_address = gpu.pci_address.substr(gpu.pci_address.rfind('/') + 1);
        gpus.push_back(gpu);
    }
    // NVIDIA's memory only matters against a GPU with as much as NVIDIA is
    // assumed to have (a discrete AMD GPU); NVML wakes the GPU, so only then.
    const bool rival = std::any_of(gpus.begin(), gpus.end(), [](const LinuxGpu& g) {
        return g.vendor != kVendorNvidia && g.vram_bytes >= kAssumedNvidiaVram;
    });
    if (rival) {
        for (LinuxGpu& g : gpus) {
            if (g.vendor == kVendorNvidia && g.vram_bytes == 0) g.vram_bytes = nvidia_vram(g.pci_address);
        }
    }
    return gpus;
}

static uint64_t ranking_vram(const LinuxGpu& g) {
    if (g.vendor == kVendorNvidia && g.vram_bytes == 0) return kAssumedNvidiaVram;
    return g.vram_bytes;
}

const LinuxGpu* video_gpu(const std::vector<LinuxGpu>& gpus) {
    const LinuxGpu* best = nullptr;
    for (const LinuxGpu& g : gpus) {
        if (!best || ranking_vram(g) > ranking_vram(*best) ||
            (ranking_vram(g) == ranking_vram(*best) && g.primary)) {
            best = &g;
        }
    }
    return best;
}

std::vector<std::string> linux_decoder_order(const std::vector<LinuxGpu>& gpus, bool soft,
                                             const std::string& forced) {
    if (soft) return {"OpenH264"};
    const LinuxGpu* gpu = video_gpu(gpus);
    std::vector<std::string> order;
    if (gpu && gpu->vendor == kVendorNvidia) {
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

std::vector<std::string> linux_encoder_order(const std::vector<LinuxGpu>& gpus, bool soft,
                                             const std::string& forced) {
    if (soft || forced == "openh264") return {"OpenH264"};
    const LinuxGpu* gpu = video_gpu(gpus);
    std::vector<std::string> order;
    if (gpu && gpu->vendor == kVendorNvidia) {
        order = {"NVENC", "VA-API"};
    } else {
        order = {"VA-API", "NVENC"};
    }
    const std::string first = forced == "nvenc" ? "NVENC" : forced == "va" ? "VA-API" : "";
    if (!first.empty() && order.front() != first) std::swap(order[0], order[1]);
    return order;
}

std::string video_render_node(const std::vector<LinuxGpu>& gpus) {
    const LinuxGpu* gpu = video_gpu(gpus);
    return gpu ? gpu->render_node : std::string{};
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

std::string va_driver_for_video_gpu(const std::vector<LinuxGpu>& gpus,
                                    const std::vector<std::string>& driver_dirs) {
    if (gpus.size() < 2) return {};
    const LinuxGpu* gpu = video_gpu(gpus);
    if (!gpu) return {};
    // Two GPUs of one vendor share one VA driver; naming it changes nothing.
    std::vector<std::string> candidates;
    if (gpu->vendor == kVendorAmd) candidates = {"radeonsi"};
    if (gpu->vendor == kVendorIntel) candidates = {"iHD", "i965"};
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
        // The NVENC path's GL runs headless: no window, so no window
        // surface (an EGL window surface failed with EGL_BAD_ALLOC there).
        setenv("GST_GL_WINDOW", "surfaceless", 0);
        if (std::getenv("LIBVA_DRIVER_NAME")) return;
        const std::string driver = va_driver_for_video_gpu(list_linux_gpus(), va_driver_dirs());
        if (driver.empty()) return;
        setenv("LIBVA_DRIVER_NAME", driver.c_str(), 0);
        MELLO_LOG_INFO("video/decoder", "VA-API: LIBVA_DRIVER_NAME=%s (the video GPU's driver: GStreamer's VA "
                                        "plugin does not open the other GPU)", driver.c_str());
    });
}

} // namespace mello::video
#endif
