#include "decoder_factory.hpp"
#include "../util/log.hpp"

#ifdef _WIN32
#include "decoder_nvdec.hpp"
#include "decoder_amf.hpp"
#include "decoder_d3d11va.hpp"
#include "decoder_openh264.hpp"
#include "decoder_dav1d.hpp"
#elif defined(__APPLE__)
#include "decoder_videotoolbox.hpp"
#endif

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

std::unique_ptr<Decoder> create_best_decoder(
    const GraphicsDevice& device,
    const DecoderConfig&  config)
{
#ifdef _WIN32
    // 1. NVDEC (HW)
    if (NvdecDecoder::is_available()) {
        auto dec = std::make_unique<NvdecDecoder>();
        if (dec->initialize(device, config)) return dec;
    } else {
        MELLO_LOG_DEBUG(TAG, "Probing NVDEC... not available (no NVIDIA GPU)");
    }

    // 2. AMF (HW)
    if (AmfDecoder::is_available()) {
        auto dec = std::make_unique<AmfDecoder>();
        if (dec->initialize(device, config)) return dec;
    } else {
        MELLO_LOG_DEBUG(TAG, "Probing AMF decode... not available (AMD driver not found)");
    }

    // 3. D3D11VA (Intel + generic HW)
    if (D3d11vaDecoder::is_available(device.d3d11())) {
        auto dec = std::make_unique<D3d11vaDecoder>();
        if (dec->initialize(device, config)) return dec;
    } else {
        MELLO_LOG_DEBUG(TAG, "Probing D3D11VA... not available");
    }

    // 4. Software fallback — codec-dependent
    if (config.codec == VideoCodec::AV1) {
        if (Dav1dDecoder::is_available()) {
            auto dec = std::make_unique<Dav1dDecoder>();
            if (dec->initialize(device, config)) return dec;
        } else {
            MELLO_LOG_WARN(TAG, "dav1d not available — AV1 software decode disabled");
        }
    } else {
        if (OpenH264Decoder::is_available()) {
            auto dec = std::make_unique<OpenH264Decoder>();
            if (dec->initialize(device, config)) return dec;
        } else {
            MELLO_LOG_WARN(TAG, "OpenH264 DLL not found — H.264 software decode disabled");
        }
    }
#elif defined(__APPLE__)
    if (VTDecoder::is_available()) {
        MELLO_LOG_INFO(TAG, "Probing VideoToolbox decoder...");
        auto dec = std::make_unique<VTDecoder>();
        if (dec->initialize(device, config)) {
            MELLO_LOG_INFO(TAG, "Selected decoder: VideoToolbox codec=H264 %ux%u", config.width, config.height);
            return dec;
        }
        MELLO_LOG_WARN(TAG, "VideoToolbox: initialize() failed");
    }
    MELLO_LOG_ERROR(TAG, "No decoder available on macOS");
#else
    (void)device; (void)config;
    MELLO_LOG_ERROR(TAG, "No decoders available on this platform");
#endif

    return nullptr;
}

std::vector<const char*> enumerate_decoders(const GraphicsDevice& device) {
    std::vector<const char*> result;

#ifdef _WIN32
    if (NvdecDecoder::is_available())                    result.push_back("NVDEC");
    if (AmfDecoder::is_available())                      result.push_back("AMF-Decode");
    if (D3d11vaDecoder::is_available(device.d3d11()))    result.push_back("D3D11VA");
    if (OpenH264Decoder::is_available())                 result.push_back("OpenH264");
    if (Dav1dDecoder::is_available())                    result.push_back("dav1d");
#elif defined(__APPLE__)
    (void)device;
    if (VTDecoder::is_available()) result.push_back("VideoToolbox");
#else
    (void)device;
#endif

    return result;
}

} // namespace mello::video
