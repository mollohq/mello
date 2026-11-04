#pragma once

// Big-endian bytes and ISO BMFF boxes, for the game clip MP4 writer and demux
// (CLIPS.md 15.5 and 15.9). Portable: no OS call.

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <vector>

namespace mello::replay::mp4 {

inline uint16_t be16(const uint8_t* p) { return static_cast<uint16_t>((p[0] << 8) | p[1]); }
inline uint32_t be32(const uint8_t* p) {
    return (uint32_t{p[0]} << 24) | (uint32_t{p[1]} << 16) | (uint32_t{p[2]} << 8) | p[3];
}
inline uint64_t be64(const uint8_t* p) { return (uint64_t{be32(p)} << 32) | be32(p + 4); }

inline uint32_t fourcc(const char* s) {
    return (uint32_t{static_cast<uint8_t>(s[0])} << 24) | (uint32_t{static_cast<uint8_t>(s[1])} << 16) |
           (uint32_t{static_cast<uint8_t>(s[2])} << 8) | static_cast<uint8_t>(s[3]);
}

/// Bytes in big-endian order, with nested boxes: `begin` writes a size
/// placeholder and the type, `end` patches the size of the box it closes.
class Writer {
public:
    void u8(uint8_t v) { bytes_.push_back(v); }
    void u16(uint16_t v) {
        u8(static_cast<uint8_t>(v >> 8));
        u8(static_cast<uint8_t>(v));
    }
    void u24(uint32_t v) {
        u8(static_cast<uint8_t>(v >> 16));
        u16(static_cast<uint16_t>(v));
    }
    void u32(uint32_t v) {
        u16(static_cast<uint16_t>(v >> 16));
        u16(static_cast<uint16_t>(v));
    }
    void u64(uint64_t v) {
        u32(static_cast<uint32_t>(v >> 32));
        u32(static_cast<uint32_t>(v));
    }
    void zeros(size_t n) { bytes_.insert(bytes_.end(), n, 0); }
    void data(const void* p, size_t n) {
        const auto* b = static_cast<const uint8_t*>(p);
        bytes_.insert(bytes_.end(), b, b + n);
    }
    void type(const char* t) { data(t, 4); }

    void begin(const char* t) {
        open_.push_back(bytes_.size());
        u32(0);
        type(t);
    }
    /// A full box: the version and the 24-bit flags after the type.
    void begin_full(const char* t, uint8_t version, uint32_t flags) {
        begin(t);
        u8(version);
        u24(flags);
    }
    void end() {
        const size_t at = open_.back();
        open_.pop_back();
        const auto size = static_cast<uint32_t>(bytes_.size() - at);
        bytes_[at] = static_cast<uint8_t>(size >> 24);
        bytes_[at + 1] = static_cast<uint8_t>(size >> 16);
        bytes_[at + 2] = static_cast<uint8_t>(size >> 8);
        bytes_[at + 3] = static_cast<uint8_t>(size);
    }

    const std::vector<uint8_t>& bytes() const { return bytes_; }
    size_t size() const { return bytes_.size(); }

private:
    std::vector<uint8_t> bytes_;
    std::vector<size_t> open_;
};

/// One box inside a byte range: its type, and its body after the header.
struct BoxRef {
    uint32_t type = 0;
    const uint8_t* body = nullptr;
    size_t body_size = 0;
};

/// Calls `fn(box)` for each box in [data, data + size), in order. False when
/// a box header or size does not fit the range.
template <typename Fn>
bool for_each_box(const uint8_t* data, size_t size, Fn&& fn) {
    size_t at = 0;
    while (at < size) {
        if (size - at < 8) return false;
        uint64_t box = be32(data + at);
        size_t header = 8;
        if (box == 1) {
            if (size - at < 16) return false;
            box = be64(data + at + 8);
            header = 16;
        } else if (box == 0) {
            box = size - at;
        }
        if (box < header || box > size - at) return false;
        BoxRef ref;
        ref.type = be32(data + at + 4);
        ref.body = data + at + header;
        ref.body_size = static_cast<size_t>(box) - header;
        fn(ref);
        at += static_cast<size_t>(box);
    }
    return true;
}

}  // namespace mello::replay::mp4
