#pragma once

// The A/V sync header extension of the game audio (streaming-quality stage
// 7, spec 12 section 9.4).
//
// Each game-audio RTP packet of a stream host carries the video RTP
// timestamp (90 kHz) of the instant its audio was captured: the timestamp a
// video frame captured at that instant gets. The viewer so maps its audio
// clock onto the video clock with no sender report (the SFU forwards none).
//
// RFC 8285 one-byte form, element ID 5, 4 bytes, network order. The SFU does
// not negotiate the URI; it forwards the packet header unchanged
// (mello-sfu 01-SFU.md, the stream audio relay), so both ends use the fixed
// ID.

#include <cstddef>
#include <cstdint>
#include <optional>
#include <vector>

namespace mello::transport {

inline constexpr uint8_t kAvSyncExtensionId = 5;
inline constexpr char kAvSyncExtensionUri[] = "urn:mello:params:rtp-hdrext:av-sync-video-ts";

namespace detail {

inline size_t rtp_fixed_header_size(const uint8_t* p, size_t size) {
    if (!p || size < 12 || (p[0] >> 6) != 2) return 0;
    const size_t header = 12 + static_cast<size_t>(p[0] & 0x0F) * 4;
    return header <= size ? header : 0;
}

} // namespace detail

/// Add the element to an RTP packet. A packet without an extension gets a
/// one-byte-form block; a one-byte-form block gets the element appended.
/// Returns false (and leaves the packet as it was) for another form or a
/// malformed packet.
inline bool add_av_sync_extension(std::vector<std::byte>& packet, uint32_t video_rtp_timestamp) {
    auto* p = reinterpret_cast<uint8_t*>(packet.data());
    const size_t header = detail::rtp_fixed_header_size(p, packet.size());
    if (header == 0) return false;
    // The element and its padding to a 32-bit boundary: 2 words.
    const std::byte element[8] = {
        std::byte{static_cast<uint8_t>((kAvSyncExtensionId << 4) | 3)},
        std::byte{static_cast<uint8_t>(video_rtp_timestamp >> 24)},
        std::byte{static_cast<uint8_t>(video_rtp_timestamp >> 16)},
        std::byte{static_cast<uint8_t>(video_rtp_timestamp >> 8)},
        std::byte{static_cast<uint8_t>(video_rtp_timestamp)},
        std::byte{0}, std::byte{0}, std::byte{0},
    };
    const bool has_extension = (p[0] & 0x10) != 0;
    if (!has_extension) {
        const std::byte block[4] = {std::byte{0xBE}, std::byte{0xDE}, std::byte{0x00},
                                    std::byte{0x02}};
        std::vector<std::byte> insert(block, block + 4);
        insert.insert(insert.end(), element, element + 8);
        packet.insert(packet.begin() + static_cast<std::ptrdiff_t>(header), insert.begin(),
                      insert.end());
        packet[0] = packet[0] | std::byte{0x10};
        return true;
    }
    if (header + 4 > packet.size() || p[header] != 0xBE || p[header + 1] != 0xDE) return false;
    const size_t words = (static_cast<size_t>(p[header + 2]) << 8) | p[header + 3];
    const size_t end = header + 4 + words * 4;
    if (end > packet.size() || words + 2 > 0xFFFF) return false;
    packet.insert(packet.begin() + static_cast<std::ptrdiff_t>(end), element, element + 8);
    p = reinterpret_cast<uint8_t*>(packet.data());
    p[header + 2] = static_cast<uint8_t>((words + 2) >> 8);
    p[header + 3] = static_cast<uint8_t>(words + 2);
    return true;
}

/// The element of an RTP packet (one-byte or two-byte form), or empty.
inline std::optional<uint32_t> read_av_sync_extension(const uint8_t* p, size_t size) {
    const size_t header = detail::rtp_fixed_header_size(p, size);
    if (header == 0 || (p[0] & 0x10) == 0 || header + 4 > size) return std::nullopt;
    const uint16_t profile = static_cast<uint16_t>((p[header] << 8) | p[header + 1]);
    const size_t words = (static_cast<size_t>(p[header + 2]) << 8) | p[header + 3];
    size_t i = header + 4;
    const size_t end = i + words * 4;
    if (end > size) return std::nullopt;
    const bool one_byte = profile == 0xBEDE;
    const bool two_byte = (profile & 0xFFF0) == 0x1000;
    if (!one_byte && !two_byte) return std::nullopt;
    while (i < end) {
        if (p[i] == 0) {  // padding
            ++i;
            continue;
        }
        uint8_t id = 0;
        size_t len = 0;
        if (one_byte) {
            id = p[i] >> 4;
            len = static_cast<size_t>(p[i] & 0x0F) + 1;
            if (id == 15) break;
            ++i;
        } else {
            if (i + 2 > end) break;
            id = p[i];
            len = p[i + 1];
            i += 2;
        }
        if (i + len > end) break;
        if (id == kAvSyncExtensionId && len == 4) {
            return (static_cast<uint32_t>(p[i]) << 24) | (static_cast<uint32_t>(p[i + 1]) << 16) |
                   (static_cast<uint32_t>(p[i + 2]) << 8) | p[i + 3];
        }
        i += len;
    }
    return std::nullopt;
}

} // namespace mello::transport
