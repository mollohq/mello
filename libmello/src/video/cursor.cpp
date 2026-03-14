#include "cursor.hpp"
#include <cstring>
#include <algorithm>

namespace mello::video {

static constexpr uint8_t CURSOR_SUBTYPE = 0x02;

// Wire format:
//   [0]    subtype (0x02)
//   [1-4]  x (int32 LE)
//   [5-8]  y (int32 LE)
//   [9]    visible
//   [10]   shape_changed
//   If shape_changed:
//     [11-12] shape_w (uint16 LE)
//     [13-14] shape_h (uint16 LE)
//     [15..]  shape_data (shape_w * shape_h * 4 bytes, RGBA)

static void write_i32(uint8_t* p, int32_t v) {
    memcpy(p, &v, 4);
}

static void write_u16(uint8_t* p, uint16_t v) {
    memcpy(p, &v, 2);
}

static int32_t read_i32(const uint8_t* p) {
    int32_t v;
    memcpy(&v, p, 4);
    return v;
}

static uint16_t read_u16(const uint8_t* p) {
    uint16_t v;
    memcpy(&v, p, 2);
    return v;
}

size_t serialize_cursor_packet(const CursorState& state, bool include_shape,
                               uint8_t* buf, size_t buf_size) {
    size_t header_size = 11;
    size_t shape_size = 0;

    if (include_shape && state.shape_w > 0 && state.shape_h > 0) {
        shape_size = 4 + static_cast<size_t>(state.shape_w) * state.shape_h * 4;
    }

    size_t total = header_size + shape_size;
    if (buf_size < total) return 0;

    buf[0] = CURSOR_SUBTYPE;
    write_i32(buf + 1, state.x);
    write_i32(buf + 5, state.y);
    buf[9]  = state.visible ? 1 : 0;
    buf[10] = (shape_size > 0) ? 1 : 0;

    if (shape_size > 0) {
        write_u16(buf + 11, state.shape_w);
        write_u16(buf + 13, state.shape_h);
        size_t pixel_bytes = static_cast<size_t>(state.shape_w) * state.shape_h * 4;
        size_t copy_len = std::min(pixel_bytes, state.shape_rgba.size());
        if (copy_len > 0) {
            memcpy(buf + 15, state.shape_rgba.data(), copy_len);
        }
        if (copy_len < pixel_bytes) {
            memset(buf + 15 + copy_len, 0, pixel_bytes - copy_len);
        }
    }

    return total;
}

bool deserialize_cursor_packet(const uint8_t* buf, size_t size, CursorState& out) {
    if (size < 11) return false;
    if (buf[0] != CURSOR_SUBTYPE) return false;

    out.x       = read_i32(buf + 1);
    out.y       = read_i32(buf + 5);
    out.visible = buf[9] != 0;

    bool shape_changed = buf[10] != 0;
    if (shape_changed) {
        if (size < 15) return false;
        out.shape_w = read_u16(buf + 11);
        out.shape_h = read_u16(buf + 13);
        size_t pixel_bytes = static_cast<size_t>(out.shape_w) * out.shape_h * 4;
        if (size < 15 + pixel_bytes) return false;
        out.shape_rgba.assign(buf + 15, buf + 15 + pixel_bytes);
    }

    return true;
}

} // namespace mello::video
