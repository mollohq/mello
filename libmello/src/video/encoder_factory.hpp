#pragma once
#include "encoder.hpp"
#include <memory>
#include <vector>

namespace mello::video {

/// Priority order: NVENC -> AMF -> QSV (oneVPL).
/// No software fallback — returns nullptr if no HW encoder is available.
std::unique_ptr<Encoder> create_best_encoder(
    const GraphicsDevice& device,
    const EncoderConfig&  config
);

/// Returns all encoder backend names available on this machine (HW only).
std::vector<const char*> enumerate_encoders(const GraphicsDevice& device);

} // namespace mello::video
