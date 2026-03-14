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
        auto enc = std::make_unique<NvencEncoder>();
        if (enc->initialize(device, config)) return enc;
    } else {
        MELLO_LOG_DEBUG(TAG, "Probing NVENC... not available (no NVIDIA GPU)");
    }

    if (AmfEncoder::is_available()) {
        auto enc = std::make_unique<AmfEncoder>();
        if (enc->initialize(device, config)) return enc;
    } else {
        MELLO_LOG_DEBUG(TAG, "Probing AMF... not available (AMD driver not found)");
    }

    if (QsvEncoder::is_available()) {
        auto enc = std::make_unique<QsvEncoder>();
        if (enc->initialize(device, config)) return enc;
    } else {
        MELLO_LOG_DEBUG(TAG, "Probing QSV... not available (oneVPL runtime missing)");
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
