#include "twcc.hpp"

#include <algorithm>
#include <cmath>

namespace mello::transport {
namespace {

constexpr uint16_t kOneByteHeaderProfile = 0xBEDE;
constexpr uint8_t kTwccFeedbackFmt = 15;
constexpr uint8_t kRtcpRtpfb = 205;
constexpr int64_t kReferenceTimeUnitUs = 64'000;
constexpr int64_t kDeltaUnitUs = 250;
// One report stays under the path MTU with the SRTCP and DTLS overhead.
constexpr size_t kMaxReportBytes = 1'100;

uint16_t read_u16(const uint8_t* p) {
    return static_cast<uint16_t>((static_cast<uint16_t>(p[0]) << 8) | p[1]);
}

uint32_t read_u32(const uint8_t* p) {
    return (static_cast<uint32_t>(p[0]) << 24)
        | (static_cast<uint32_t>(p[1]) << 16)
        | (static_cast<uint32_t>(p[2]) << 8)
        | static_cast<uint32_t>(p[3]);
}

uint32_t read_u24(const uint8_t* p) {
    return (static_cast<uint32_t>(p[0]) << 16)
        | (static_cast<uint32_t>(p[1]) << 8)
        | static_cast<uint32_t>(p[2]);
}

void write_u16(uint8_t* p, uint16_t v) {
    p[0] = static_cast<uint8_t>(v >> 8);
    p[1] = static_cast<uint8_t>(v & 0xff);
}

void write_u24(uint8_t* p, uint32_t v) {
    p[0] = static_cast<uint8_t>((v >> 16) & 0xff);
    p[1] = static_cast<uint8_t>((v >> 8) & 0xff);
    p[2] = static_cast<uint8_t>(v & 0xff);
}

void write_u32(uint8_t* p, uint32_t v) {
    p[0] = static_cast<uint8_t>(v >> 24);
    p[1] = static_cast<uint8_t>((v >> 16) & 0xff);
    p[2] = static_cast<uint8_t>((v >> 8) & 0xff);
    p[3] = static_cast<uint8_t>(v & 0xff);
}

struct RtpHeaderInfo {
    size_t header_size = 0; // fixed header + CSRC
    size_t ext_body_offset = 0; // absolute offset of extension body, 0 if none
    size_t ext_body_size = 0;
    bool has_extension = false;
    bool valid = false;
};

RtpHeaderInfo inspect_rtp_header(const uint8_t* data, size_t size) {
    RtpHeaderInfo info;
    if (data == nullptr || size < 12 || (data[0] >> 6) != 2) {
        return info;
    }
    const size_t csrc = data[0] & 0x0f;
    const size_t header_size = 12 + csrc * 4;
    if (header_size > size) {
        return info;
    }
    info.header_size = header_size;
    info.has_extension = (data[0] & 0x10) != 0;
    if (info.has_extension) {
        if (header_size + 4 > size) {
            return info;
        }
        const size_t ext_words = read_u16(data + header_size + 2);
        info.ext_body_offset = header_size + 4;
        info.ext_body_size = ext_words * 4;
        if (info.ext_body_offset + info.ext_body_size > size) {
            return info;
        }
    }
    info.valid = true;
    return info;
}

// Offset of our TWCC element's value bytes within the extension body, or -1.
int find_twcc_element(const uint8_t* body, size_t size) {
    size_t i = 0;
    while (i < size) {
        const uint8_t header = body[i];
        if (header == 0) {
            ++i; // padding
            continue;
        }
        const int id = header >> 4;
        const int len = (header & 0x0f) + 1;
        if (id == 15) {
            break; // reserved: stop parsing per RFC 8285
        }
        if (id == kTwccExtensionId && len == 2 && i + 2 < size) {
            return static_cast<int>(i + 1);
        }
        i += 1 + static_cast<size_t>(len);
    }
    return -1;
}

int16_t read_s16(const uint8_t* p) {
    return static_cast<int16_t>(read_u16(p));
}

// Rounds toward minus infinity, so a negative time keeps a positive delta.
int64_t floor_div(int64_t value, int64_t divisor) {
    int64_t quotient = value / divisor;
    if ((value % divisor != 0) && ((value < 0) != (divisor < 0))) {
        --quotient;
    }
    return quotient;
}

} // namespace

bool TwccSendStamper::stamp(std::vector<uint8_t>& packet, uint16_t& out_sequence) {
    const RtpHeaderInfo info = inspect_rtp_header(packet.data(), packet.size());
    if (!info.valid) {
        return false;
    }
    const uint16_t sequence = next_sequence_;

    if (info.has_extension) {
        const uint8_t* ext_header = packet.data() + info.header_size;
        if (read_u16(ext_header) == kOneByteHeaderProfile) {
            uint8_t* body = packet.data() + info.ext_body_offset;
            const int existing = find_twcc_element(body, info.ext_body_size);
            if (existing >= 0) {
                // Re-stamp (retransmit): overwrite the value in place.
                write_u16(body + existing, sequence);
            } else {
                // No room handling beyond whole-word append: extension bodies
                // are word-padded, so grow by one word and append the element.
                const size_t insert_at = info.ext_body_offset + info.ext_body_size;
                packet.insert(packet.begin() + static_cast<ptrdiff_t>(insert_at), 4, 0);
                // ext_body_offset unchanged; header length grows by one word.
                uint8_t* length_field = packet.data() + info.header_size + 2;
                const uint16_t words =
                    static_cast<uint16_t>(info.ext_body_size / 4 + 1);
                write_u16(length_field, words);
                uint8_t* new_body = packet.data() + insert_at;
                new_body[0] = static_cast<uint8_t>((kTwccExtensionId << 4) | 1);
                write_u16(new_body + 1, sequence);
                new_body[3] = 0;
            }
        } else {
            // Two-byte-header profile or unknown: do not disturb the packet.
            return false;
        }
    } else {
        // Insert an 8-byte extension block after the fixed header.
        packet[0] |= 0x10;
        const size_t at = info.header_size;
        packet.insert(packet.begin() + static_cast<ptrdiff_t>(at), 8, 0);
        uint8_t* ext = packet.data() + at;
        write_u16(ext, kOneByteHeaderProfile);
        write_u16(ext + 2, 1); // one 32-bit word of body
        ext[4] = static_cast<uint8_t>((kTwccExtensionId << 4) | 1);
        write_u16(ext + 5, sequence);
        ext[7] = 0; // padding
    }

    out_sequence = sequence;
    ++next_sequence_;
    return true;
}

bool extract_twcc_sequence(
    const uint8_t* data,
    size_t size,
    uint16_t& out_sequence
) {
    const RtpHeaderInfo info = inspect_rtp_header(data, size);
    if (!info.valid || !info.has_extension) {
        return false;
    }
    if (read_u16(data + info.header_size) != kOneByteHeaderProfile) {
        return false;
    }
    const int value_offset =
        find_twcc_element(data + info.ext_body_offset, info.ext_body_size);
    if (value_offset < 0) {
        return false;
    }
    out_sequence = read_u16(data + info.ext_body_offset + value_offset);
    return true;
}

bool parse_twcc_feedback(
    const uint8_t* data,
    size_t size,
    TwccFeedback& out
) {
    if (data == nullptr || size < 20) {
        return false;
    }
    if ((data[0] >> 6) != 2 || (data[0] & 0x1f) != kTwccFeedbackFmt
        || data[1] != kRtcpRtpfb) {
        return false;
    }
    const size_t packet_size = (static_cast<size_t>(read_u16(data + 2)) + 1) * 4;
    if (packet_size > size || packet_size < 20) {
        return false;
    }

    out.sender_ssrc = read_u32(data + 4);
    out.media_ssrc = read_u32(data + 8);
    const uint16_t base_sequence = read_u16(data + 12);
    const uint16_t status_count = read_u16(data + 14);
    // Reference time: 24 bits in 64 ms units. It wraps after about 12 days;
    // the sender unwraps it against the previous report.
    out.reference_time_64ms = read_u24(data + 16);
    out.feedback_packet_count = data[19];
    out.packets.clear();
    out.packets.reserve(status_count);

    // First pass: the packet status chunks.
    std::vector<uint8_t> symbols;
    symbols.reserve(status_count);
    size_t offset = 20;
    while (symbols.size() < status_count) {
        if (offset + 2 > packet_size) {
            return false;
        }
        const uint16_t chunk = read_u16(data + offset);
        offset += 2;
        const size_t remaining = status_count - symbols.size();
        if ((chunk & 0x8000) == 0) {
            // Run length chunk: T=0, S (2 bits), run length (13 bits).
            const uint8_t symbol = static_cast<uint8_t>((chunk >> 13) & 0x03);
            const size_t run = chunk & 0x1fff;
            if (symbol == 3 || run == 0) {
                return false;
            }
            for (size_t i = 0; i < run && i < remaining; ++i) {
                symbols.push_back(symbol);
            }
        } else if ((chunk & 0x4000) == 0) {
            // Status vector chunk, 14 one-bit symbols.
            for (size_t i = 0; i < 14 && i < remaining; ++i) {
                symbols.push_back(static_cast<uint8_t>((chunk >> (13 - i)) & 0x01));
            }
        } else {
            // Status vector chunk, 7 two-bit symbols.
            for (size_t i = 0; i < 7 && i < remaining; ++i) {
                const uint8_t symbol =
                    static_cast<uint8_t>((chunk >> (12 - i * 2)) & 0x03);
                if (symbol == 3) {
                    return false;
                }
                symbols.push_back(symbol);
            }
        }
    }

    // Second pass: one delta per received packet, in sequence order. A
    // small delta is unsigned 8 bits, a large delta signed 16 bits, both in
    // 250 us units. The first delta is relative to the reference time.
    int64_t arrival_us =
        static_cast<int64_t>(out.reference_time_64ms) * kReferenceTimeUnitUs;
    for (size_t i = 0; i < symbols.size(); ++i) {
        TwccPacketResult result;
        result.sequence = static_cast<uint16_t>(base_sequence + i);
        const uint8_t symbol = symbols[i];
        if (symbol == 1) {
            if (offset + 1 > packet_size) {
                return false;
            }
            arrival_us += static_cast<int64_t>(data[offset]) * kDeltaUnitUs;
            offset += 1;
            result.received = true;
            result.arrival_time_us = arrival_us;
        } else if (symbol == 2) {
            if (offset + 2 > packet_size) {
                return false;
            }
            arrival_us += static_cast<int64_t>(read_s16(data + offset)) * kDeltaUnitUs;
            offset += 2;
            result.received = true;
            result.arrival_time_us = arrival_us;
        }
        out.packets.push_back(result);
    }
    return true;
}

void TwccFeedbackGenerator::on_packet(uint16_t twcc_sequence, int64_t arrival_time_us) {
    if (has_emitted_) {
        // Ignore sequences at or behind the last emitted packet: they can no
        // longer be reported (already covered) or are duplicates.
        const int16_t ahead =
            static_cast<int16_t>(twcc_sequence - last_emitted_sequence_);
        if (ahead <= 0) {
            return;
        }
    }
    pending_[twcc_sequence] = Arrival{arrival_time_us};
}

namespace {

// Status symbols of the transport-cc draft.
constexpr uint8_t kNotReceived = 0;
constexpr uint8_t kSmallDelta = 1;
constexpr uint8_t kLargeDelta = 2;

void push_u16(std::vector<uint8_t>& out, uint16_t value) {
    out.push_back(static_cast<uint8_t>(value >> 8));
    out.push_back(static_cast<uint8_t>(value & 0xff));
}

// Encodes `symbols` into packet status chunks. A run of at least 14 equal
// symbols, or a run of at least 7 that ends the list, is a run length chunk.
// Else a one-bit vector of 14 when the next 14 symbols have no large delta;
// else a two-bit vector of 7.
std::vector<uint8_t> encode_status_chunks(const std::vector<uint8_t>& symbols) {
    std::vector<uint8_t> chunks;
    size_t i = 0;
    const size_t n = symbols.size();
    while (i < n) {
        size_t run = 1;
        while (i + run < n && symbols[i + run] == symbols[i] && run < 0x1fff) {
            ++run;
        }
        if (run >= 14 || (i + run == n && run >= 7)) {
            push_u16(chunks, static_cast<uint16_t>(
                (static_cast<uint16_t>(symbols[i]) << 13) | run));
            i += run;
            continue;
        }
        const size_t one_bit = std::min<size_t>(14, n - i);
        bool fits_one_bit = true;
        for (size_t j = 0; j < one_bit; ++j) {
            if (symbols[i + j] == kLargeDelta) {
                fits_one_bit = false;
                break;
            }
        }
        if (fits_one_bit) {
            uint16_t chunk = 0x8000;
            for (size_t j = 0; j < one_bit; ++j) {
                if (symbols[i + j] == kSmallDelta) {
                    chunk |= static_cast<uint16_t>(1u << (13 - j));
                }
            }
            push_u16(chunks, chunk);
            i += one_bit;
            continue;
        }
        const size_t two_bit = std::min<size_t>(7, n - i);
        uint16_t chunk = 0xc000;
        for (size_t j = 0; j < two_bit; ++j) {
            chunk |= static_cast<uint16_t>(
                static_cast<uint16_t>(symbols[i + j]) << (12 - j * 2));
        }
        push_u16(chunks, chunk);
        i += two_bit;
    }
    return chunks;
}

} // namespace

std::vector<uint8_t> TwccFeedbackGenerator::build_feedback(
    uint32_t sender_ssrc,
    uint32_t media_ssrc
) {
    if (pending_.empty()) {
        return {};
    }

    uint16_t base = 0;
    if (has_emitted_) {
        base = static_cast<uint16_t>(last_emitted_sequence_ + 1);
    } else {
        base = pending_.begin()->first;
        for (const auto& [sequence, _] : pending_) {
            if (static_cast<int16_t>(sequence - base) < 0) {
                base = sequence;
            }
        }
    }
    uint16_t newest = base;
    for (const auto& [sequence, _] : pending_) {
        if (static_cast<int16_t>(sequence - newest) > 0) {
            newest = sequence;
        }
    }
    const size_t span =
        static_cast<size_t>(static_cast<uint16_t>(newest - base)) + 1;

    // The first received packet sets the reference time: its arrival time
    // rounded down to 64 ms. The first delta is that arrival time minus the
    // same rounded value, so the reference and the deltas are on one clock.
    int64_t first_arrival_us = 0;
    for (size_t j = 0; j < span; ++j) {
        const auto it = pending_.find(static_cast<uint16_t>(base + j));
        if (it != pending_.end()) {
            first_arrival_us = it->second.at_us;
            break;
        }
    }
    const int64_t reference_units =
        floor_div(first_arrival_us, kReferenceTimeUnitUs);
    // The arrival time the receiver of this report reconstructs: the
    // reference plus the sum of the quantised deltas. Each delta is taken
    // against it, so the rounding does not add up over a report.
    int64_t clock_us = reference_units * kReferenceTimeUnitUs;

    // Walk the span in sequence order. The report ends at the last received
    // packet that fits: it ends earlier when one more packet makes it larger
    // than kMaxReportBytes, or when a delta does not fit in 16 bits.
    std::vector<uint8_t> symbols;
    std::vector<uint8_t> deltas;
    size_t emit_count = 0;
    for (size_t j = 0; j < span; ++j) {
        const auto it = pending_.find(static_cast<uint16_t>(base + j));
        if (it == pending_.end()) {
            symbols.push_back(kNotReceived);
            continue;
        }
        const int64_t delta_units =
            floor_div(it->second.at_us - clock_us, kDeltaUnitUs);
        if (delta_units < -32768 || delta_units > 32767) {
            break;
        }
        const bool small = delta_units >= 0 && delta_units <= 255;
        const size_t delta_bytes = deltas.size() + (small ? 1 : 2);
        // Two bytes of chunk for every 7 symbols is the worst case.
        const size_t chunk_bound = (symbols.size() + 1 + 6) / 7 * 2;
        if (emit_count > 0 && 20 + chunk_bound + delta_bytes > kMaxReportBytes) {
            break;
        }
        if (small) {
            symbols.push_back(kSmallDelta);
            deltas.push_back(static_cast<uint8_t>(delta_units));
        } else {
            symbols.push_back(kLargeDelta);
            push_u16(deltas, static_cast<uint16_t>(static_cast<int16_t>(delta_units)));
        }
        clock_us += delta_units * kDeltaUnitUs;
        emit_count = symbols.size();
    }
    if (emit_count == 0) {
        // Not possible: the first received packet sets the reference, so
        // its delta is under 64 ms. Drop the state rather than loop.
        pending_.clear();
        return {};
    }
    symbols.resize(emit_count);
    const std::vector<uint8_t> chunks = encode_status_chunks(symbols);

    // Header(4) + SSRCs(8) + base(2) + count(2) + reference(3) + count(1) =
    // 20 bytes, then the chunks and the deltas, padded with zeros to 32 bits.
    const size_t body = 20 + chunks.size() + deltas.size();
    const size_t padded = (body + 3) & ~size_t{3};
    std::vector<uint8_t> packet(padded, 0);
    packet[0] = static_cast<uint8_t>(0x80 | kTwccFeedbackFmt);
    packet[1] = kRtcpRtpfb;
    write_u16(packet.data() + 2, static_cast<uint16_t>(padded / 4 - 1));
    write_u32(packet.data() + 4, sender_ssrc);
    write_u32(packet.data() + 8, media_ssrc);
    write_u16(packet.data() + 12, base);
    write_u16(packet.data() + 14, static_cast<uint16_t>(emit_count));
    write_u24(packet.data() + 16,
              static_cast<uint32_t>(reference_units & 0x00ff'ffff));
    packet[19] = feedback_packet_count_++;
    std::copy(chunks.begin(), chunks.end(), packet.begin() + 20);
    std::copy(
        deltas.begin(),
        deltas.end(),
        packet.begin() + 20 + static_cast<ptrdiff_t>(chunks.size()));

    for (size_t j = 0; j < emit_count; ++j) {
        pending_.erase(static_cast<uint16_t>(base + j));
    }
    last_emitted_sequence_ = static_cast<uint16_t>(base + emit_count - 1);
    has_emitted_ = true;
    return packet;
}

} // namespace mello::transport
