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

bool pointer_shape_to_bgra(PointerShapeKind kind, uint32_t width, uint32_t height,
                           uint32_t pitch, const uint8_t* data, size_t size,
                           std::vector<uint8_t>& out) {
    if (!data || width == 0 || height == 0) return false;
    out.assign(static_cast<size_t>(width) * height * 4, 0);

    if (kind == PointerShapeKind::Monochrome) {
        if (pitch * 8 < width || size < static_cast<size_t>(pitch) * height * 2) return false;
        for (uint32_t y = 0; y < height; ++y) {
            const uint8_t* and_row = data + static_cast<size_t>(y) * pitch;
            const uint8_t* xor_row = data + static_cast<size_t>(y + height) * pitch;
            for (uint32_t x = 0; x < width; ++x) {
                const uint8_t bit = static_cast<uint8_t>(0x80 >> (x % 8));
                const bool and_bit = (and_row[x / 8] & bit) != 0;
                const bool xor_bit = (xor_row[x / 8] & bit) != 0;
                uint8_t* p = &out[(static_cast<size_t>(y) * width + x) * 4];
                if (and_bit && !xor_bit) continue;  // the screen shows through
                // AND 0: the XOR bit is the colour (white or black). AND 1 and
                // XOR 1 inverts the screen: black (see the header).
                const uint8_t v = (!and_bit && xor_bit) ? 255 : 0;
                p[0] = p[1] = p[2] = v;
                p[3] = 255;
            }
        }
        return true;
    }

    if (kind != PointerShapeKind::Color && kind != PointerShapeKind::MaskedColor) return false;
    if (pitch < width * 4 || size < static_cast<size_t>(pitch) * (height - 1) + width * 4) return false;
    for (uint32_t y = 0; y < height; ++y) {
        const uint8_t* row = data + static_cast<size_t>(y) * pitch;
        for (uint32_t x = 0; x < width; ++x) {
            const uint8_t* s = row + static_cast<size_t>(x) * 4;
            uint8_t* p = &out[(static_cast<size_t>(y) * width + x) * 4];
            if (kind == PointerShapeKind::Color) {
                p[0] = s[0]; p[1] = s[1]; p[2] = s[2]; p[3] = s[3];
            } else if (s[3] == 0) {
                p[0] = s[0]; p[1] = s[1]; p[2] = s[2]; p[3] = 255;  // replaces the screen
            } else if (s[0] | s[1] | s[2]) {
                p[0] = p[1] = p[2] = 0; p[3] = 255;                  // inverts: black
            }                                                         // XOR 0: transparent
        }
    }
    return true;
}

std::vector<uint8_t> scale_bgra(const std::vector<uint8_t>& src, uint32_t src_w, uint32_t src_h,
                                uint32_t dst_w, uint32_t dst_h) {
    std::vector<uint8_t> out(static_cast<size_t>(dst_w) * dst_h * 4, 0);
    if (src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 ||
        src.size() < static_cast<size_t>(src_w) * src_h * 4) {
        return out;
    }
    for (uint32_t y = 0; y < dst_h; ++y) {
        const float fy = (static_cast<float>(y) + 0.5f) * src_h / dst_h - 0.5f;
        const int y0 = std::max(0, std::min(static_cast<int>(src_h) - 1, static_cast<int>(fy)));
        const int y1 = std::min(static_cast<int>(src_h) - 1, y0 + 1);
        const float wy = std::max(0.0f, std::min(1.0f, fy - y0));
        for (uint32_t x = 0; x < dst_w; ++x) {
            const float fx = (static_cast<float>(x) + 0.5f) * src_w / dst_w - 0.5f;
            const int x0 = std::max(0, std::min(static_cast<int>(src_w) - 1, static_cast<int>(fx)));
            const int x1 = std::min(static_cast<int>(src_w) - 1, x0 + 1);
            const float wx = std::max(0.0f, std::min(1.0f, fx - x0));
            const uint8_t* q[4] = {
                &src[(static_cast<size_t>(y0) * src_w + x0) * 4],
                &src[(static_cast<size_t>(y0) * src_w + x1) * 4],
                &src[(static_cast<size_t>(y1) * src_w + x0) * 4],
                &src[(static_cast<size_t>(y1) * src_w + x1) * 4],
            };
            const float w[4] = {(1 - wx) * (1 - wy), wx * (1 - wy), (1 - wx) * wy, wx * wy};
            float a = 0, c[3] = {0, 0, 0};
            for (int i = 0; i < 4; ++i) {
                const float qa = w[i] * q[i][3];
                a += qa;
                for (int k = 0; k < 3; ++k) c[k] += qa * q[i][k];
            }
            uint8_t* p = &out[(static_cast<size_t>(y) * dst_w + x) * 4];
            if (a > 0.0f) {
                for (int k = 0; k < 3; ++k) {
                    p[k] = static_cast<uint8_t>(std::min(255.0f, c[k] / a + 0.5f));
                }
            }
            p[3] = static_cast<uint8_t>(std::min(255.0f, a + 0.5f));
        }
    }
    return out;
}

} // namespace mello::video
