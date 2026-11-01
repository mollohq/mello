#pragma once
// Runtime loader for Cisco's prebuilt OpenH264 DLL: the decoder, and the
// encoder of the test lane (MELLO_SOFT_ENCODER, encoder_openh264.cpp).
// Must use the official Cisco binary — do not build from source.
// Linux: openh264_dynload_linux.cpp loads the shared library the same way.

#if defined(_WIN32) || defined(__linux__)
#include <cstdint>

namespace mello::video::openh264 {

/// Decoder entry points, typed `void**` like EncoderApi: the decoder
/// classes come from the vendored API headers in decoder_openh264.cpp.
struct DecoderApi {
    long (*create_decoder)(void** ppDecoder);
    void (*destroy_decoder)(void* pDecoder);
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
