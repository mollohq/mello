#include "encoder_factory.hpp"
#include "../util/log.hpp"
#include <cstdlib>
#include <cstring>
#ifdef _WIN32
#include <windows.h>
#endif

#ifdef _WIN32
#include "encoder_nvenc.hpp"
#include "encoder_amf.hpp"
#include "encoder_qsv.hpp"
#include "encoder_openh264.hpp"
#elif defined(__APPLE__) && !defined(MELLO_IOS_NO_HOSTING)
#include "encoder_videotoolbox.hpp"
#endif

namespace mello::video {

static constexpr const char* TAG = "video/encoder";

bool soft_encoder_requested() {
#ifdef _WIN32
    // The process environment, not the C runtime's copy: a harness that sets
    // the variable after start (Rust std::env::set_var) changes only the
    // first. Same rule as MELLO_AUDIO_BACKEND (audio_backend_test.cpp).
    char buf[8] = {};
    const DWORD n = GetEnvironmentVariableA("MELLO_SOFT_ENCODER", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
#else
    const char* v = std::getenv("MELLO_SOFT_ENCODER");
    return v && std::strcmp(v, "1") == 0;
#endif
}

std::unique_ptr<Encoder> create_best_encoder(
    const GraphicsDevice& device,
    const EncoderConfig&  config)
{
#ifdef _WIN32
    if (soft_encoder_requested()) {
        // Test lane: the hardware encoders are skipped, OpenH264 is the only
        // candidate. A failure here is a failure, never a hardware fallback.
        MELLO_LOG_INFO(TAG, "MELLO_SOFT_ENCODER=1: software OpenH264 encoder only");
        if (OpenH264Encoder::is_available()) {
            auto enc = std::make_unique<OpenH264Encoder>();
            if (enc->initialize(device, config)) return enc;
            MELLO_LOG_ERROR(TAG, "OpenH264: initialize() failed");
        } else {
            MELLO_LOG_ERROR(TAG, "OpenH264: DLL with encoder entry points not found");
        }
        return nullptr;
    }

    if (NvencEncoder::is_available()) {
        MELLO_LOG_INFO(TAG, "NVENC DLL found, attempting init...");
        auto enc = std::make_unique<NvencEncoder>();
        if (enc->initialize(device, config)) return enc;
        MELLO_LOG_WARN(TAG, "NVENC: DLL present but initialize() failed");
    } else {
        MELLO_LOG_INFO(TAG, "NVENC: nvEncodeAPI64.dll not found — skipping");
    }

    if (AmfEncoder::is_available()) {
        MELLO_LOG_INFO(TAG, "AMF DLL found, attempting init...");
        auto enc = std::make_unique<AmfEncoder>();
        if (enc->initialize(device, config)) return enc;
        MELLO_LOG_WARN(TAG, "AMF: DLL present but initialize() failed");
    } else {
        MELLO_LOG_INFO(TAG, "AMF: amfrt64.dll not found — skipping");
    }

    if (QsvEncoder::is_available()) {
        MELLO_LOG_INFO(TAG, "QSV DLL found, attempting init...");
        auto enc = std::make_unique<QsvEncoder>();
        if (enc->initialize(device, config)) return enc;
        MELLO_LOG_WARN(TAG, "QSV: DLL present but initialize() failed");
    } else {
        MELLO_LOG_INFO(TAG, "QSV: libvpl.dll not found — skipping");
    }

    MELLO_LOG_ERROR(TAG, "No hardware encoder available (NVENC, AMF, QSV all failed)");
#elif defined(__APPLE__) && !defined(MELLO_IOS_NO_HOSTING)
    if (VTEncoder::is_available()) {
        MELLO_LOG_INFO(TAG, "Probing VideoToolbox encoder...");
        auto enc = std::make_unique<VTEncoder>();
        if (enc->initialize(device, config)) {
            MELLO_LOG_INFO(TAG, "Selected encoder: VideoToolbox codec=H264 %ux%u", config.width, config.height);
            return enc;
        }
        MELLO_LOG_WARN(TAG, "VideoToolbox: initialize() failed");
    }
    MELLO_LOG_ERROR(TAG, "No encoder available on macOS");
#else
    (void)device; (void)config;
    MELLO_LOG_ERROR(TAG, "No encoders available on this platform");
#endif

    return nullptr;
}

std::vector<const char*> enumerate_encoders(const GraphicsDevice& device) {
    std::vector<const char*> result;
    (void)device;

#ifdef _WIN32
    if (soft_encoder_requested()) {
        if (OpenH264Encoder::is_available()) result.push_back("OpenH264");
        return result;
    }
    if (NvencEncoder::is_available()) result.push_back("NVENC");
    if (AmfEncoder::is_available())   result.push_back("AMF");
    if (QsvEncoder::is_available())   result.push_back("QSV-oneVPL");
#elif defined(__APPLE__) && !defined(MELLO_IOS_NO_HOSTING)
    if (VTEncoder::is_available()) result.push_back("VideoToolbox");
#endif

    return result;
}

} // namespace mello::video
