#if defined(__linux__)
// Linux loader for OpenH264. The same entry points as openh264_dynload.cpp,
// from a shared library, in this order: the Cisco binary the client
// downloaded (MELLO_OPENH264_PATH), Cisco's binary next to the executable,
// then the system library. All must have the ABI of the vendored v2.6.0
// headers, which is soname 8. MELLO_OPENH264_DISABLED=1 (the user turned
// the Cisco codec off) loads none. The client sets both variables at start
// (mello-core stream/openh264.rs), before any thread reads them.
#include "openh264_dynload.hpp"
#include "../util/log.hpp"

#include <cstdlib>
#include <cstring>
#include <dlfcn.h>
#include <limits.h>
#include <string>
#include <unistd.h>

namespace mello::video::openh264 {

static constexpr const char* TAG = "video/openh264";

static void*      handle_     = nullptr;
static bool       loaded_     = false;
static DecoderApi api_{};
static EncoderApi enc_api_{};
static bool       enc_loaded_ = false;

// The directory of the running executable, with a trailing slash. Empty when
// /proc/self/exe is not readable.
static std::string exe_dir() {
    char buf[PATH_MAX];
    const ssize_t n = readlink("/proc/self/exe", buf, sizeof(buf) - 1);
    if (n <= 0) return {};
    std::string path(buf, static_cast<size_t>(n));
    const size_t slash = path.rfind('/');
    return slash == std::string::npos ? std::string{} : path.substr(0, slash + 1);
}

template<typename T>
static bool load_fn(void* mod, const char* name, T& out) {
    out = reinterpret_cast<T>(dlsym(mod, name));
    if (!out) {
        MELLO_LOG_ERROR(TAG, "Failed to resolve %s", name);
        return false;
    }
    return true;
}

static bool disabled() {
    const char* v = std::getenv("MELLO_OPENH264_DISABLED");
    return v && std::strcmp(v, "1") == 0;
}

bool load() {
    if (loaded_) return true;
    if (disabled()) {
        static bool logged = false;
        if (!logged) {
            logged = true;
            MELLO_LOG_INFO(TAG, "OpenH264 turned off in the settings, H.264 software decoder unavailable");
        }
        return false;
    }

    std::string from;
    if (const char* downloaded = std::getenv("MELLO_OPENH264_PATH"); downloaded && *downloaded) {
        handle_ = dlopen(downloaded, RTLD_NOW | RTLD_LOCAL);
        if (handle_) from = downloaded;
    }
    const std::string dir = exe_dir();
    const std::string bundled[] = {
        dir + "libopenh264-2.6.0-linux64.8.so",
        dir + "libopenh264-2.5.0-linux64.8.so",
    };
    if (!handle_ && !dir.empty()) {
        for (const std::string& path : bundled) {
            handle_ = dlopen(path.c_str(), RTLD_NOW | RTLD_LOCAL);
            if (handle_) { from = path; break; }
        }
    }
    if (!handle_) {
        handle_ = dlopen("libopenh264.so.8", RTLD_NOW | RTLD_LOCAL);
        if (handle_) from = "libopenh264.so.8";
    }
    if (!handle_) {
        MELLO_LOG_WARN(TAG, "OpenH264 library not found, H.264 software decoder unavailable");
        return false;
    }

    bool ok = true;
    ok &= load_fn(handle_, "WelsCreateDecoder",  api_.create_decoder);
    ok &= load_fn(handle_, "WelsDestroyDecoder", api_.destroy_decoder);
    if (!ok) {
        MELLO_LOG_ERROR(TAG, "Failed to resolve OpenH264 symbols in %s", from.c_str());
        unload();
        return false;
    }

    // The encoder is optional: a library without it still decodes.
    enc_api_.create_encoder = reinterpret_cast<int (*)(void**)>(dlsym(handle_, "WelsCreateSVCEncoder"));
    enc_api_.destroy_encoder = reinterpret_cast<void (*)(void*)>(dlsym(handle_, "WelsDestroySVCEncoder"));
    enc_loaded_ = enc_api_.create_encoder && enc_api_.destroy_encoder;
    if (!enc_loaded_) {
        enc_api_ = {};
        MELLO_LOG_WARN(TAG, "OpenH264 library has no encoder entry points");
    }

    loaded_ = true;
    MELLO_LOG_INFO(TAG, "OpenH264 loaded: %s", from.c_str());
    return true;
}

void unload() {
    if (handle_) { dlclose(handle_); handle_ = nullptr; }
    api_ = {};
    enc_api_ = {};
    enc_loaded_ = false;
    loaded_ = false;
}

bool is_loaded() { return loaded_; }

const DecoderApi& api() { return api_; }

bool encoder_api_loaded() { return loaded_ && enc_loaded_; }

const EncoderApi& encoder_api() { return enc_api_; }

} // namespace mello::video::openh264
#endif
