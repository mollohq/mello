#pragma once
#include <algorithm>
#include <cstddef>
#include <cstdint>

namespace mello::video {

/// How NVDEC copies one decoded NV12 surface into a host buffer.
///
/// The CUDA surface has its chroma plane at row `stream_coded_h` (the coded
/// height of the stream, a multiple of 16). The host buffer has a fixed
/// layout, set at initialize: `layout_rows` luma rows, then the chroma plane.
/// The luma and the chroma are copied as two blocks, so the copy never goes
/// past the buffer and the chroma lands where the texture expects it,
/// whatever coded height the stream has.
struct NvdecHostCopy {
    uint32_t luma_rows      = 0;  // rows copied from row 0 to row 0
    uint32_t chroma_src_row = 0;  // first chroma row in the CUDA surface
    uint32_t chroma_dst_row = 0;  // first chroma row in the host buffer
    uint32_t chroma_rows    = 0;
    size_t   buffer_bytes   = 0;  // the size of the host buffer

    /// Bytes the two copies write, for a buffer with `pitch` bytes per row.
    size_t bytes_written(uint32_t pitch) const {
        return static_cast<size_t>(pitch) * (chroma_dst_row + chroma_rows);
    }
};

/// `width` is the host row pitch in bytes (one byte per luma sample).
/// `layout_rows` is the luma height of the host buffer and its texture: the
/// macroblock-aligned height for the R8 path, the display height for the
/// NV12 path. `stream_coded_h` comes from the sequence callback.
inline NvdecHostCopy plan_nvdec_host_copy(uint32_t width, uint32_t layout_rows,
                                          uint32_t stream_coded_h) {
    NvdecHostCopy plan;
    const uint32_t layout = layout_rows & ~1u;
    plan.luma_rows      = (std::min)(stream_coded_h, layout) & ~1u;
    plan.chroma_src_row = stream_coded_h;
    plan.chroma_dst_row = layout;
    plan.chroma_rows    = plan.luma_rows / 2;
    plan.buffer_bytes   = static_cast<size_t>(width) * (layout + layout / 2);
    return plan;
}

} // namespace mello::video
