#pragma once
#if defined(__linux__)
#include "cpu_frame.hpp"
#include "decoder.hpp"
#include <cstdint>

namespace mello::video {

/// Converts an I420 or NV12 picture to RGBA8 (alpha 255) in `rgba`, which holds
/// `width * height * 4` bytes, tightly packed. BT.709 studio swing, with the
/// integer coefficients of the Windows viewer shader (staging_texture.cpp),
/// so a stream has the same colors on both. Each value rounds to nearest.
///
/// Thread: any. The present thread of the Linux viewer calls it.
void i420_to_rgba(const CpuPicture& pic, uint8_t* rgba);

/// Converts a 32-bit RGB picture to I420 at `dst_w` x `dst_h` (even), BT.709
/// studio swing, as the hosts' encoders signal it. A size change samples the
/// nearest source pixel; each chroma sample averages its four luma pixels.
/// `y`, `u`, `v` are tight planes (strides dst_w and dst_w / 2).
///
/// Thread: any. The Linux software encoder calls it on the capture thread.
void rgbx_to_i420(const CpuFrame& src, uint32_t dst_w, uint32_t dst_h, uint8_t* y, uint8_t* u, uint8_t* v);

} // namespace mello::video
#endif
