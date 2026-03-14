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

} // namespace mello::video
