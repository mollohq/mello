#pragma once
#include <cstdint>
#include <vector>

namespace mello::video {

struct CursorState {
    int32_t  x = 0;
    int32_t  y = 0;
    bool     visible = false;
    uint16_t shape_w = 0;
    uint16_t shape_h = 0;
    std::vector<uint8_t> shape_rgba;
};

// Serialize cursor state into a binary packet for the control DataChannel.
// Position-only packets are ~10 bytes; shape packets include the full RGBA bitmap.
size_t serialize_cursor_packet(const CursorState& state, bool include_shape,
                               uint8_t* buf, size_t buf_size);

// Deserialize a cursor packet received from the host.
bool deserialize_cursor_packet(const uint8_t* buf, size_t size, CursorState& out);

/// The pointer shape kinds of desktop duplication
/// (`DXGI_OUTDUPL_POINTER_SHAPE_TYPE_*`, same values). A Win32 cursor is one
/// of them too: a monochrome cursor has no colour bitmap.
enum class PointerShapeKind : uint32_t {
    Monochrome  = 1,  ///< 1 bpp AND mask, then 1 bpp XOR mask, `height` rows each
    Color       = 2,  ///< 32 bpp BGRA with alpha
    MaskedColor = 4,  ///< 32 bpp BGRA, alpha 0 replaces, alpha 0xFF XORs
};

/// Turns a pointer shape into straight-alpha BGRA, `width * height * 4` bytes.
///
/// `height` is the height of the pointer, not of the data: a monochrome shape
/// has `2 * height` rows. `pitch` is the bytes per row of `data`.
///
/// A pixel the shape inverts (XOR with the screen) has no alpha-blend form.
/// It becomes opaque black: the inverting pointers (the text I-beam) are shown
/// over light text fields, where black is what the user sees. Pure.
bool pointer_shape_to_bgra(PointerShapeKind kind, uint32_t width, uint32_t height,
                           uint32_t pitch, const uint8_t* data, size_t size,
                           std::vector<uint8_t>& out);

/// Bilinear scale of a straight-alpha BGRA bitmap. The colour is weighted by
/// alpha, so a transparent pixel does not darken the edge next to it. Pure.
std::vector<uint8_t> scale_bgra(const std::vector<uint8_t>& src, uint32_t src_w, uint32_t src_h,
                                uint32_t dst_w, uint32_t dst_h);

/// The largest pointer side a cursor packet carries. A larger shape is scaled
/// down, so a packet stays under 64 KiB on the reliable control channel.
constexpr uint32_t kMaxCursorPacketSide = 128;

/// The largest cursor packet, for the caller's buffer.
constexpr size_t kMaxCursorPacketBytes = 15 + kMaxCursorPacketSide * kMaxCursorPacketSide * 4;

} // namespace mello::video
