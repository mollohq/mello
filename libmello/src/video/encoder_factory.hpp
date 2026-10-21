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

/// True when MELLO_SOFT_ENCODER is "1": the test lane of the streaming quality
/// gate (tools/stream-gate) encodes with OpenH264 in software. Read on each
/// call, so a test can set it before a host starts.
bool soft_encoder_requested();

/// Returns all encoder backend names available on this machine. Hardware
/// only, or only "OpenH264" when soft_encoder_requested().
std::vector<const char*> enumerate_encoders(const GraphicsDevice& device);

} // namespace mello::video
