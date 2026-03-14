#include "encoder_factory.hpp"
#include "../util/log.hpp"

#ifdef _WIN32
#include "encoder_nvenc.hpp"
#include "encoder_amf.hpp"
#include "encoder_qsv.hpp"
#endif

namespace mello::video {

static constexpr const char* TAG = "video/encoder";

std::unique_ptr<Encoder> create_best_encoder(
    const GraphicsDevice& device,
    const EncoderConfig&  config)
{
#ifdef _WIN32
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
    if (NvencEncoder::is_available()) result.push_back("NVENC");
    if (AmfEncoder::is_available())   result.push_back("AMF");
    if (QsvEncoder::is_available())   result.push_back("QSV-oneVPL");
#endif

    return result;
}

} // namespace mello::video
