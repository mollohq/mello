#pragma once
#include "decoder.hpp"
#include <memory>
#include <vector>

namespace mello::video {

/// Priority order: NVDEC -> AMF -> D3D11VA -> OpenH264 (H.264) / dav1d (AV1)
std::unique_ptr<Decoder> create_best_decoder(
    const GraphicsDevice& device,
    const DecoderConfig&  config
);

/// Returns all decoder backend names available on this machine.
std::vector<const char*> enumerate_decoders(const GraphicsDevice& device);

} // namespace mello::video
