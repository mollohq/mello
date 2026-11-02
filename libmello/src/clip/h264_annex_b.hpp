#pragma once

// H.264 samples from an MP4 for a decoder that wants Annex B: the game clip
// player and the stream gate's read-back of a game clip.

#include <cstddef>
#include <cstdint>
#include <vector>

namespace mello::clip {

inline bool has_start_code(const std::vector<uint8_t>& b) {
    return (b.size() >= 4 && b[0] == 0 && b[1] == 0 && b[2] == 0 && b[3] == 1) ||
           (b.size() >= 3 && b[0] == 0 && b[1] == 0 && b[2] == 1);
}

/// Media Foundation's MPEG-4 source gives H.264 in Annex B. A sample with
/// 4-byte lengths (AVCC) is converted, to be safe; anything else stays.
inline std::vector<uint8_t> to_annex_b(std::vector<uint8_t> bytes) {
    if (bytes.empty() || has_start_code(bytes)) return bytes;
    std::vector<uint8_t> out;
    out.reserve(bytes.size() + 16);
    size_t at = 0;
    while (at + 4 <= bytes.size()) {
        const size_t n = (size_t{bytes[at]} << 24) | (size_t{bytes[at + 1]} << 16) |
                         (size_t{bytes[at + 2]} << 8) | bytes[at + 3];
        at += 4;
        if (n == 0 || at + n > bytes.size()) return bytes;  // not AVCC after all
        static const uint8_t kStart[] = {0, 0, 0, 1};
        out.insert(out.end(), kStart, kStart + 4);
        out.insert(out.end(), bytes.begin() + static_cast<std::ptrdiff_t>(at),
                   bytes.begin() + static_cast<std::ptrdiff_t>(at + n));
        at += n;
    }
    return at == bytes.size() ? out : bytes;
}

}  // namespace mello::clip
