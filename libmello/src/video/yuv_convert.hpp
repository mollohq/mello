#pragma once
#if defined(__linux__)
#include "decoder.hpp"
#include <cstdint>

namespace mello::video {

/// Converts an I420 picture to RGBA8 (alpha 255) in `rgba`, which holds
/// `width * height * 4` bytes, tightly packed. BT.709 studio swing, with the
/// integer coefficients of the Windows viewer shader (staging_texture.cpp),
/// so a stream has the same colors on both. Each value rounds to nearest.
///
/// Thread: any. The present thread of the Linux viewer calls it.
void i420_to_rgba(const CpuPicture& pic, uint8_t* rgba);

} // namespace mello::video
#endif
