#include "ulpfec.hpp"

namespace mello::transport {
namespace {

uint16_t read_u16(const uint8_t* p) {
    return static_cast<uint16_t>((static_cast<uint16_t>(p[0]) << 8) | p[1]);
}

uint32_t read_u32(const uint8_t* p) {
    return (static_cast<uint32_t>(p[0]) << 24)
        | (static_cast<uint32_t>(p[1]) << 16)
        | (static_cast<uint32_t>(p[2]) << 8)
        | static_cast<uint32_t>(p[3]);
}

void write_u16(uint8_t* p, uint16_t v) {
    p[0] = static_cast<uint8_t>(v >> 8);
    p[1] = static_cast<uint8_t>(v & 0xff);
}

void write_u32(uint8_t* p, uint32_t v) {
    p[0] = static_cast<uint8_t>(v >> 24);
    p[1] = static_cast<uint8_t>((v >> 16) & 0xff);
    p[2] = static_cast<uint8_t>((v >> 8) & 0xff);
    p[3] = static_cast<uint8_t>(v & 0xff);
}

struct ParsedRtp {
    uint16_t sequence = 0;
    uint32_t timestamp = 0;
    uint8_t payload_type = 0;
    bool marker = false;
    const uint8_t* payload = nullptr;
    size_t payload_size = 0;
    bool valid = false;
};

// Minimal parse for FEC purposes: fixed 12-byte header + CSRC (no extension
// support needed — generator feeds pre-stamp packets, recovery skips the
// extension bytes on buffered media packets via the word count).
ParsedRtp parse_rtp(const uint8_t* data, size_t size) {
    ParsedRtp out;
    if (data == nullptr || size < 12 || (data[0] >> 6) != 2) {
        return out;
    }
    const size_t csrc = data[0] & 0x0f;
    size_t header = 12 + csrc * 4;
    if (header > size) {
        return out;
    }
    if ((data[0] & 0x10) != 0) {
        if (header + 4 > size) {
            return out;
        }
        const size_t words = read_u16(data + header + 2);
        header += 4 + words * 4;
        if (header > size) {
            return out;
        }
    }
    out.sequence = read_u16(data + 2);
    out.timestamp = read_u32(data + 4);
    out.payload_type = data[1] & 0x7f;
    out.marker = (data[1] & 0x80) != 0;
    out.payload = data + header;
    out.payload_size = size - header;
    out.valid = true;
    return out;
}

void xor_into(std::vector<uint8_t>& dst, const uint8_t* src, size_t size) {
    if (dst.size() < size) {
        dst.resize(size, 0);
    }
    for (size_t i = 0; i < size; ++i) {
        dst[i] ^= src[i];
    }
}

} // namespace

UlpfecGenerator::UlpfecGenerator(size_t group_size)
    : group_size_(group_size > kMaxFecGroupSize ? kMaxFecGroupSize : group_size) {}

void UlpfecGenerator::add_packet(const uint8_t* data, size_t size) {
    const ParsedRtp pkt = parse_rtp(data, size);
    if (!pkt.valid || ready_) {
        return;
    }

    if (count_ == 0) {
        sn_base_ = pkt.sequence;
        mask_ = 0;
        ts_recovery_ = 0;
        length_recovery_ = 0;
        parity_.clear();
        contiguous_ = true;
    } else if (pkt.sequence != static_cast<uint16_t>(last_seq_ + 1)) {
        // Group must be contiguous for a 16-bit mask; abandon it rather
        // than emit an FEC packet that cannot decode correctly.
        contiguous_ = false;
    }

    const size_t index = static_cast<uint16_t>(pkt.sequence - sn_base_);
    if (index < 16) {
        mask_ |= static_cast<uint16_t>(0x8000u >> index);
    } else {
        contiguous_ = false;
    }
    ts_recovery_ ^= pkt.timestamp;
    length_recovery_ ^= static_cast<uint16_t>(pkt.payload_size);
    xor_into(parity_, pkt.payload, pkt.payload_size);

    last_seq_ = pkt.sequence;
    last_ts_ = pkt.timestamp;
    last_marker_ = pkt.marker;
    if (++count_ >= group_size_) {
        // Group complete: retain it for exactly one build_packet() call.
        ready_ = true;
        count_ = 0;
    }
}

std::vector<uint8_t> UlpfecGenerator::build_packet(
    uint32_t media_ssrc,
    uint32_t rtp_timestamp
) {
    std::vector<uint8_t> packet;
    if (!ready_) {
        return packet;
    }
    ready_ = false;
    if (!contiguous_) {
        parity_.clear();
        return packet;
    }

    packet.resize(12 + 12 + parity_.size());
    uint8_t* p = packet.data();
    // RTP header.
    p[0] = 0x80;
    p[1] = static_cast<uint8_t>((last_marker_ ? 0x80 : 0) | kUlpfecPayloadType);
    write_u16(p + 2, fec_sequence_++);
    write_u32(p + 4, rtp_timestamp);
    write_u32(p + 8, media_ssrc + 1);
    // ULPFEC header (12 bytes, level 0): E=L=P=X=CC=0; M mirrors the last
    // protected packet's marker; PT recovery = original media PT (96);
    // SN base; TS recovery; length recovery; 16-bit mask.
    p[12] = 0;
    p[13] = static_cast<uint8_t>((last_marker_ ? 0x80 : 0) | (96 & 0x7f));
    write_u16(p + 14, sn_base_);
    write_u32(p + 16, ts_recovery_);
    write_u16(p + 20, length_recovery_);
    write_u16(p + 22, mask_);
    std::copy(parity_.begin(), parity_.end(), p + 24);

    parity_.clear();
    return packet;
}

void UlpfecRecovery::remember_media(
    uint16_t sequence,
    std::vector<uint8_t> packet
) {
    if (media_.count(sequence) != 0) {
        return;
    }
    media_order_.push_back(sequence);
    media_.emplace(sequence, std::move(packet));
    while (media_order_.size() > kMaxMediaPackets) {
        media_.erase(media_order_.front());
        media_order_.pop_front();
    }
}

void UlpfecRecovery::add_media_packet(const uint8_t* data, size_t size) {
    const ParsedRtp pkt = parse_rtp(data, size);
    if (!pkt.valid) {
        return;
    }
    remember_media(
        pkt.sequence,
        std::vector<uint8_t>(data, data + size)
    );
}

void UlpfecRecovery::add_fec_packet(const uint8_t* data, size_t size) {
    if (data == nullptr || size < 12 + 12) {
        return;
    }
    const ParsedRtp pkt = parse_rtp(data, size);
    if (!pkt.valid || pkt.payload_type != kUlpfecPayloadType
        || pkt.payload_size < 12) {
        return;
    }
    // The parity stream rides media SSRC + 1; learn the media SSRC from it.
    media_ssrc_ = read_u32(data + 8) - 1;

    const uint8_t* h = pkt.payload;
    FecBlock block;
    block.marker = (h[1] & 0x80) != 0;
    block.pt_recovery = h[1] & 0x7f;
    block.sn_base = read_u16(h + 2);
    block.ts_recovery = read_u32(h + 4);
    block.length_recovery = read_u16(h + 8);
    block.mask = read_u16(h + 10);
    // Recovery payload is everything after the 12-byte header; its length
    // equals the longest protected payload in the group.
    block.recovery_payload.assign(h + 12, h + pkt.payload_size);
    fec_.push_back(std::move(block));
    while (fec_.size() > kMaxFecBlocks) {
        fec_.pop_front();
    }
}

std::vector<uint16_t> UlpfecRecovery::uncovered_mask_sequences() const {
    std::vector<uint16_t> uncovered;
    for (const FecBlock& block : fec_) {
        for (size_t i = 0; i < 16; ++i) {
            if ((block.mask & (0x8000u >> i)) == 0) {
                continue;
            }
            const uint16_t sequence =
                static_cast<uint16_t>(block.sn_base + i);
            if (media_.count(sequence) == 0) {
                uncovered.push_back(sequence);
            }
        }
    }
    return uncovered;
}

bool UlpfecRecovery::recover(
    uint16_t sequence,
    std::vector<uint8_t>& out
) {
    for (const FecBlock& block : fec_) {
        const int16_t index =
            static_cast<int16_t>(sequence - block.sn_base);
        if (index < 0 || index >= 16) {
            continue;
        }
        if ((block.mask & (0x8000u >> index)) == 0) {
            continue;
        }

        // Count present members of this group.
        size_t missing = 0;
        size_t missing_index = 0;
        for (size_t i = 0; i < 16; ++i) {
            if ((block.mask & (0x8000u >> i)) == 0) {
                continue;
            }
            const uint16_t seq =
                static_cast<uint16_t>(block.sn_base + i);
            if (media_.count(seq) == 0) {
                ++missing;
                missing_index = i;
            }
        }
        if (missing != 1
            || static_cast<uint16_t>(block.sn_base + missing_index)
                != sequence) {
            if (missing > 1) {
                ++unrecoverable_;
            }
            continue;
        }

        // XOR the fields of all present members against the recovery data.
        uint32_t timestamp = block.ts_recovery;
        uint16_t length = block.length_recovery;
        std::vector<uint8_t> payload = block.recovery_payload;
        bool marker = false;
        uint8_t payload_type = block.pt_recovery;
        size_t highest_index = 0;
        for (size_t i = 0; i < 16; ++i) {
            if ((block.mask & (0x8000u >> i)) != 0) {
                highest_index = i;
            }
        }

        for (size_t i = 0; i < 16; ++i) {
            if ((block.mask & (0x8000u >> i)) == 0 || i == missing_index) {
                continue;
            }
            const uint16_t seq =
                static_cast<uint16_t>(block.sn_base + i);
            const ParsedRtp present =
                parse_rtp(media_.at(seq).data(), media_.at(seq).size());
            if (!present.valid) {
                return false;
            }
            timestamp ^= present.timestamp;
            length ^= static_cast<uint16_t>(present.payload_size);
            xor_into(payload, present.payload, present.payload_size);
        }

        if (length > payload.size()) {
            // Recovered length exceeds the padded recovery payload —
            // the group data is inconsistent; treat as unrecoverable.
            ++unrecoverable_;
            return false;
        }
        payload.resize(length);
        if (missing_index == highest_index) {
            marker = block.marker;
        }

        out.resize(12 + length);
        out[0] = 0x80;
        out[1] = static_cast<uint8_t>((marker ? 0x80 : 0) | payload_type);
        write_u16(out.data() + 2, sequence);
        write_u32(out.data() + 4, timestamp);
        write_u32(out.data() + 8, media_ssrc_);
        std::copy(payload.begin(), payload.end(), out.begin() + 12);

        // Cache the reconstruction as received: the same sequence is never
        // rebuilt twice, and later mask scans no longer see it as missing.
        remember_media(sequence, out);
        ++recovered_;
        return true;
    }
    return false;
}

} // namespace mello::transport
