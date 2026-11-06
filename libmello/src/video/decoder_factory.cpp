#include "decoder_factory.hpp"
#include "../util/log.hpp"
#include <cstdlib>
#include <cstring>
#ifdef _WIN32
#include <windows.h>
#endif

#ifdef _WIN32
#include "decoder_nvdec.hpp"
#include "decoder_amf.hpp"
#include "decoder_mf.hpp"
#include "decoder_openh264.hpp"
#include <algorithm>
#include <string>
#elif defined(__APPLE__)
#include "decoder_videotoolbox.hpp"
#elif defined(__linux__)
#include "decoder_openh264.hpp"
#include "decoder_nvdec.hpp"
#include "decoder_gst_va.hpp"
#include "linux_gpu.hpp"
#include <algorithm>
#include <string>
#include <thread>
#endif

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

#if defined(__linux__)
static std::unique_ptr<Decoder> make_linux_decoder(const std::string& name) {
    if (name == "NVDEC") return std::make_unique<NvdecDecoder>();
    if (name == "VA-API") return std::make_unique<GstVaDecoder>();
    if (name == "OpenH264") return std::make_unique<OpenH264Decoder>();
    return nullptr;
}

static std::vector<std::string> linux_order() {
    const char* forced = std::getenv("MELLO_LINUX_DECODER");
    return linux_decoder_order(list_linux_gpus(), soft_decoder_requested(), forced ? forced : "");
}

void warm_linux_decoders() {
    // Only the first decoder: warming NVDEC would wake an NVIDIA GPU that
    // the order keeps asleep. Detached like the Windows hook offsets
    // (mello.cpp): nothing waits for it.
    prepare_va_driver_env();
    const std::vector<std::string> order = linux_order();
    if (!order.empty() && order.front() == "VA-API") {
        std::thread([] { GstVaDecoder::warm_up(); }).detach();
    }
}

std::unique_ptr<Decoder> create_next_decoder(const GraphicsDevice& device, const DecoderConfig& config,
                                             const std::vector<std::string>& tried) {
    if (config.codec != VideoCodec::H264) {
        MELLO_LOG_ERROR(TAG, "No decoder available: Linux decodes H.264 only");
        return nullptr;
    }
    for (const std::string& name : linux_order()) {
        if (std::find(tried.begin(), tried.end(), name) != tried.end()) continue;
        auto dec = make_linux_decoder(name);
        if (dec && dec->initialize(device, config)) return dec;
    }
    MELLO_LOG_ERROR(TAG, "No decoder available: NVDEC, VA-API and OpenH264 all failed");
    return nullptr;
}
#endif

#if defined(_WIN32)
std::vector<std::string> windows_decoder_order(bool soft, const std::string& forced) {
    if (soft) return {"OpenH264"};
    std::vector<std::string> order = {"NVDEC", "AMF-Decode", "MediaFoundation", "OpenH264"};
    const char* first = nullptr;
    if (forced == "nvdec") first = "NVDEC";
    else if (forced == "amf") first = "AMF-Decode";
    else if (forced == "mf") first = "MediaFoundation";
    else if (forced == "openh264") first = "OpenH264";
    else if (!forced.empty()) {
        MELLO_LOG_WARN(TAG, "MELLO_WINDOWS_DECODER=%s is not nvdec, amf, mf or openh264: ignored", forced.c_str());
    }
    if (first) {
        order.erase(std::find(order.begin(), order.end(), first));
        order.insert(order.begin(), first);
    }
    return order;
}

static std::vector<std::string> windows_order() {
    // The process environment, as soft_decoder_requested().
    char buf[32] = {};
    const DWORD n = GetEnvironmentVariableA("MELLO_WINDOWS_DECODER", buf, sizeof(buf));
    const std::string forced = (n > 0 && n < sizeof(buf)) ? std::string(buf, n) : std::string();
    return windows_decoder_order(soft_decoder_requested(), forced);
}

static std::unique_ptr<Decoder> make_windows_decoder(const std::string& name) {
    if (name == "NVDEC") {
        if (NvdecDecoder::is_available()) return std::make_unique<NvdecDecoder>();
        MELLO_LOG_DEBUG(TAG, "Probing NVDEC... not available (no NVIDIA GPU)");
    } else if (name == "AMF-Decode") {
        if (AmfDecoder::is_available()) return std::make_unique<AmfDecoder>();
        MELLO_LOG_DEBUG(TAG, "Probing AMF decode... not available (AMD driver not found)");
    } else if (name == "MediaFoundation") {
        if (MfDecoder::is_available()) return std::make_unique<MfDecoder>();
        MELLO_LOG_DEBUG(TAG, "Probing Media Foundation decoder... not available");
    } else if (name == "OpenH264") {
        if (OpenH264Decoder::is_available()) return std::make_unique<OpenH264Decoder>();
        MELLO_LOG_DEBUG(TAG, "Probing OpenH264... DLL not found");
    }
    return nullptr;
}

std::unique_ptr<Decoder> create_next_decoder(const GraphicsDevice& device, const DecoderConfig& config,
                                             const std::vector<std::string>& tried) {
    // AV1 is never negotiated (signaling requires H264; VideoCodec::AV1 is
    // never assigned), so no AV1 decoder ships. dav1d was removed for
    // installed size.
    if (config.codec != VideoCodec::H264) {
        MELLO_LOG_ERROR(TAG, "No decoder available: Windows decodes H.264 only");
        return nullptr;
    }
    for (const std::string& name : windows_order()) {
        if (std::find(tried.begin(), tried.end(), name) != tried.end()) continue;
        auto dec = make_windows_decoder(name);
        if (dec && dec->initialize(device, config)) return dec;
    }
    if (soft_decoder_requested()) {
        MELLO_LOG_ERROR(TAG, "MELLO_SOFT_DECODER=1 but OpenH264 cannot decode here");
    } else if (!NvdecDecoder::is_available() && !AmfDecoder::is_available() && !MfDecoder::is_available()) {
        MELLO_LOG_ERROR(TAG, "No H.264 decoder: no NVDEC, no AMF, and no Media Foundation H.264 decoder. "
                             "Install the Media Feature Pack");
    } else {
        MELLO_LOG_ERROR(TAG, "No decoder available: NVDEC, AMF, Media Foundation and OpenH264 all failed");
    }
    return nullptr;
}
#endif

bool soft_decoder_requested() {
#ifdef _WIN32
    // The process environment, not the C runtime's copy: a harness that sets
    // the variable after start (Rust std::env::set_var) changes only the
    // first. Same rule as MELLO_AUDIO_BACKEND (audio_backend_test.cpp).
    char buf[8] = {};
    const DWORD n = GetEnvironmentVariableA("MELLO_SOFT_DECODER", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
#else
    const char* v = std::getenv("MELLO_SOFT_DECODER");
    return v && std::strcmp(v, "1") == 0;
#endif
}

std::unique_ptr<Decoder> create_best_decoder(
    const GraphicsDevice& device,
    const DecoderConfig&  config)
{
#ifdef _WIN32
    // NVDEC, AMF, then Media Foundation: an Intel GPU decodes with the
    // Microsoft H.264 decoder (12-STREAMING.md section 7.4).
    if (soft_decoder_requested()) {
        // Test lane: OpenH264 only, never a hardware decoder.
        MELLO_LOG_INFO(TAG, "MELLO_SOFT_DECODER=1: software OpenH264 decoder only");
    }
    return create_next_decoder(device, config, {});
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
#elif defined(__linux__)
    // The primary GPU's decoder first, OpenH264 last (linux_gpu.hpp).
    return create_next_decoder(device, config, {});
#else
    (void)device; (void)config;
    MELLO_LOG_ERROR(TAG, "No decoders available on this platform");
#endif

    return nullptr;
}

std::vector<const char*> enumerate_decoders(const GraphicsDevice& device) {
    std::vector<const char*> result;

#ifdef _WIN32
    (void)device;
    // Every decoder this machine has, in the default order. The soft lane
    // and the override change only the choice, as before.
    for (const std::string& name : windows_decoder_order(false, "")) {
        if (name == "NVDEC" && NvdecDecoder::is_available()) result.push_back("NVDEC");
        if (name == "AMF-Decode" && AmfDecoder::is_available()) result.push_back("AMF-Decode");
        if (name == "MediaFoundation" && MfDecoder::is_available()) result.push_back("MediaFoundation");
        if (name == "OpenH264" && OpenH264Decoder::is_available()) result.push_back("OpenH264");
    }
#elif defined(__APPLE__)
    (void)device;
    if (VTDecoder::is_available()) result.push_back("VideoToolbox");
#elif defined(__linux__)
    (void)device;
    for (const std::string& name : linux_order()) {
        if (name == "NVDEC" && NvdecDecoder::is_available()) result.push_back("NVDEC");
        if (name == "VA-API" && GstVaDecoder::is_available()) result.push_back("VA-API");
        if (name == "OpenH264" && OpenH264Decoder::is_available()) result.push_back("OpenH264");
    }
#else
    (void)device;
#endif

    return result;
}

} // namespace mello::video
