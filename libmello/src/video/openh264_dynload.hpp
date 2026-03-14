#pragma once
// Runtime loader for Cisco's prebuilt OpenH264 DLL (decoder only).
// Must use the official Cisco binary — do not build from source.

#ifdef _WIN32
#include <cstdint>

// Forward declarations matching the OpenH264 C API
struct ISVCDecoder;
struct SDecodingParam;
struct SBufferInfo;

namespace mello::video::openh264 {

struct DecoderApi {
    long (*create_decoder)(ISVCDecoder** ppDecoder);
    void (*destroy_decoder)(ISVCDecoder* pDecoder);
};

bool load();
void unload();
bool is_loaded();

const DecoderApi& api();

} // namespace mello::video::openh264
#endif
