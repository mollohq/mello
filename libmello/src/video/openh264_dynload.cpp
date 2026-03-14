#ifdef _WIN32
#include "openh264_dynload.hpp"
#include "../util/log.hpp"
#include <windows.h>

namespace mello::video::openh264 {

static constexpr const char* TAG = "video/openh264";

static HMODULE h_openh264 = nullptr;
static bool    loaded_     = false;
static DecoderApi api_{};

template<typename T>
static bool load_fn(HMODULE mod, const char* name, T& out) {
    out = reinterpret_cast<T>(GetProcAddress(mod, name));
    if (!out) {
        MELLO_LOG_ERROR(TAG, "Failed to resolve %s", name);
        return false;
    }
    return true;
}

bool load() {
    if (loaded_) return true;

    const char* names[] = { "openh264-2.6.0-win64.dll", "openh264-2.5.0-win64.dll", "openh264.dll", nullptr };
    for (int i = 0; names[i]; ++i) {
        h_openh264 = LoadLibraryA(names[i]);
        if (h_openh264) break;
    }

    if (!h_openh264) {
        MELLO_LOG_WARN(TAG, "OpenH264 DLL not found — H.264 software decoder unavailable");
        return false;
    }

    bool ok = true;
    ok &= load_fn(h_openh264, "WelsCreateDecoder",  api_.create_decoder);
    ok &= load_fn(h_openh264, "WelsDestroyDecoder", api_.destroy_decoder);

    if (!ok) {
        MELLO_LOG_ERROR(TAG, "Failed to resolve OpenH264 symbols");
        unload();
        return false;
    }

    loaded_ = true;
    MELLO_LOG_INFO(TAG, "OpenH264 Cisco DLL loaded successfully (runtime)");
    return true;
}

void unload() {
    if (h_openh264) { FreeLibrary(h_openh264); h_openh264 = nullptr; }
    api_ = {};
    loaded_ = false;
}

bool is_loaded() { return loaded_; }

const DecoderApi& api() { return api_; }

} // namespace mello::video::openh264
#endif
