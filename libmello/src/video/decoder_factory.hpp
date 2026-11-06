#pragma once
#include "decoder.hpp"
#include <memory>
#include <string>
#include <vector>

namespace mello::video {

/// The first decoder that initializes, in the platform's order (H.264 only;
/// AV1 is never negotiated, so no AV1 decoder ships). Windows:
/// windows_decoder_order(). With soft_decoder_requested() OpenH264 is the
/// only candidate.
std::unique_ptr<Decoder> create_best_decoder(
    const GraphicsDevice& device,
    const DecoderConfig&  config
);

/// True when MELLO_SOFT_DECODER is "1": the test lane of the streaming quality
/// gate decodes with OpenH264 in software on any machine. Read on each call.
bool soft_decoder_requested();

#if defined(_WIN32)
/// The Windows decoders to try, by name, in order:
/// - `soft` (MELLO_SOFT_DECODER=1): "OpenH264" alone.
/// - Else "NVDEC", "AMF-Decode", "MediaFoundation", "OpenH264". OpenH264
///   decodes only where a developer put its DLL; release builds ship none.
/// - `forced` (MELLO_WINDOWS_DECODER: nvdec, amf, mf or openh264) puts one
///   decoder first. Another value is ignored.
std::vector<std::string> windows_decoder_order(bool soft, const std::string& forced);
#endif

#if defined(__linux__) || defined(_WIN32)
/// The first decoder in the platform's order (linux_decoder_order(),
/// windows_decoder_order()) that is not in `tried` and initializes. The
/// viewer calls it again with the failed decoder's name when a decoder stops
/// working mid-stream.
std::unique_ptr<Decoder> create_next_decoder(const GraphicsDevice& device, const DecoderConfig& config,
                                             const std::vector<std::string>& tried);
#endif

#if defined(__linux__)

/// Linux: loads the first decoder's libraries in the background at start, so
/// the first watch does not wait for them. Called from mello_init.
void warm_linux_decoders();
#endif

/// Returns all decoder backend names available on this machine.
std::vector<const char*> enumerate_decoders(const GraphicsDevice& device);

} // namespace mello::video
