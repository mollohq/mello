#pragma once
#include <cstddef>
#include <cstdint>

namespace mello::replay::h264 {

constexpr uint8_t kNalIdr = 5;
constexpr uint8_t kNalSps = 7;
constexpr uint8_t kNalPps = 8;
constexpr uint8_t kNalAud = 9;
constexpr uint8_t kNalFiller = 12;

/// The size of the start code at `offset`: 4, 3, or 0 for none.
inline size_t start_code_at(const uint8_t* data, size_t size, size_t offset) {
    if (offset + 4 <= size && data[offset] == 0 && data[offset + 1] == 0 && data[offset + 2] == 0 &&
        data[offset + 3] == 1) {
        return 4;
    }
    if (offset + 3 <= size && data[offset] == 0 && data[offset + 1] == 0 && data[offset + 2] == 1) {
        return 3;
    }
    return 0;
}

/// Calls `fn(type, nal, nal_size)` for each NAL unit of an Annex-B byte
/// stream, in order. `nal` points at the NAL header byte, after the start
/// code. Returns false when the data does not start with a start code.
template <typename Fn>
bool for_each_nal(const uint8_t* data, size_t size, Fn&& fn) {
    if (!data || size == 0) return false;
    size_t sc = start_code_at(data, size, 0);
    if (sc == 0) return false;
    size_t offset = 0;
    while (offset < size) {
        const size_t nal = offset + sc;
        size_t next = nal;
        size_t next_sc = 0;
        while (next < size) {
            next_sc = start_code_at(data, size, next);
            if (next_sc != 0) break;
            ++next;
        }
        if (next > nal) fn(static_cast<uint8_t>(data[nal] & 0x1f), data + nal, next - nal);
        offset = next;
        sc = next_sc;
    }
    return true;
}

/// True when the access unit holds an IDR slice.
inline bool has_idr(const uint8_t* data, size_t size) {
    bool idr = false;
    for_each_nal(data, size, [&](uint8_t type, const uint8_t*, size_t) {
        if (type == kNalIdr) idr = true;
    });
    return idr;
}

} // namespace mello::replay::h264
