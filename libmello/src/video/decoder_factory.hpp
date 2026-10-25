#pragma once
#include "decoder.hpp"
#include <memory>
#include <vector>

namespace mello::video {

/// Priority order: NVDEC -> AMF -> OpenH264 (H.264 only; AV1 is
/// never negotiated, so no AV1 decoder ships). With soft_decoder_requested()
/// OpenH264 is the only candidate.
std::unique_ptr<Decoder> create_best_decoder(
    const GraphicsDevice& device,
    const DecoderConfig&  config
);

/// True when MELLO_SOFT_DECODER is "1": the test lane of the streaming quality
/// gate decodes with OpenH264 in software on any machine. Read on each call.
bool soft_decoder_requested();

/// Returns all decoder backend names available on this machine.
std::vector<const char*> enumerate_decoders(const GraphicsDevice& device);

} // namespace mello::video
