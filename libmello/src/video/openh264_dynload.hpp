#pragma once
// Runtime loader for Cisco's prebuilt OpenH264 DLL: the decoder, and the
// encoder of the test lane (MELLO_SOFT_ENCODER, encoder_openh264.cpp).
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

/// Encoder entry points. The pointers are typed `void**` so this header does
/// not need the OpenH264 encoder classes; encoder_openh264.cpp casts them.
struct EncoderApi {
    int  (*create_encoder)(void** ppEncoder);
    void (*destroy_encoder)(void* pEncoder);
};

/// True when load() succeeded and the DLL exports WelsCreateSVCEncoder and
/// WelsDestroySVCEncoder. The decoder does not need them.
bool encoder_api_loaded();

const EncoderApi& encoder_api();

} // namespace mello::video::openh264
#endif
