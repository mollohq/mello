#include "twcc.hpp"

#include <algorithm>
#include <cmath>

namespace mello::transport {
namespace {

constexpr uint16_t kOneByteHeaderProfile = 0xBEDE;
constexpr uint8_t kTwccFeedbackFmt = 15;
constexpr uint8_t kRtcpRtpfb = 205;
constexpr int64_t kMicrosPerSecond = 1'000'000;
// Send-side burst window: packets sent within 5 ms of each other form one
// delay-gradient measurement group (WebRTC inter-arrival grouping).
constexpr int64_t kGroupSendWindowUs = 5'000;

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

} // namespace

bool TwccSendStamper::stamp(std::vector<uint8_t>& packet, int64_t send_time_us) {
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

    {
        std::lock_guard<std::mutex> lock(mutex_);
        send_times_[sequence] = send_time_us;
        send_order_.push_back(sequence);
        while (send_order_.size() > kMaxSendRecords) {
            send_times_.erase(send_order_.front());
            send_order_.pop_front();
        }
    }
    ++next_sequence_;
    return true;
}

bool TwccSendStamper::send_time_for(uint16_t sequence, int64_t& out_us) const {
    std::lock_guard<std::mutex> lock(mutex_);
    const auto it = send_times_.find(sequence);
    if (it == send_times_.end()) {
        return false;
    }
    out_us = it->second;
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
    if (packet_size > size) {
        return false;
    }

    out.sender_ssrc = read_u32(data + 4);
    out.media_ssrc = read_u32(data + 8);
    const uint16_t base_sequence = read_u16(data + 12);
    const uint16_t status_count = read_u16(data + 14);
    // Reference time is signed 24-bit in 64 ms units.
    int32_t reference_time = static_cast<int32_t>(read_u24(data + 16));
    if ((reference_time & 0x800000) != 0) {
        reference_time -= 0x1000000;
    }
    out.feedback_packet_count = data[19];
    out.packets.clear();

    size_t chunk_offset = 20;
    size_t delta_offset = 20;
    // Deltas begin after all chunks; chunk size depends on status_count.
    // Walk chunks first to find the delta block.
    struct Symbol {
        uint8_t status;
    };
    out.packets.reserve(status_count);

    // First pass: decode symbols.
    std::vector<uint8_t> symbols;
    symbols.reserve(status_count);
    size_t covered = 0;
    size_t offset = chunk_offset;
    while (covered < status_count) {
        if (offset + 2 > packet_size) {
            return false;
        }
        const uint16_t chunk = read_u16(data + offset);
        offset += 2;
        if ((chunk & 0x8000) == 0) {
            // Run length chunk: [0][S][run:14]
            const uint8_t symbol = static_cast<uint8_t>((chunk >> 14) & 0x01);
            const size_t run = chunk & 0x3fff;
            for (size_t i = 0; i < run && covered < status_count; ++i) {
                symbols.push_back(symbol);
                ++covered;
            }
        } else {
            // Status vector chunk: [1][S][14 or 7 entries]
            const bool two_bit = ((chunk >> 14) & 0x01) != 0;
            const size_t count = two_bit ? 7 : 14;
            for (size_t i = 0; i < count && covered < status_count; ++i) {
                uint8_t symbol;
                if (two_bit) {
                    symbol = static_cast<uint8_t>((chunk >> (12 - i * 2)) & 0x03);
                } else {
                    symbol = static_cast<uint8_t>((chunk >> (13 - i)) & 0x01);
                }
                symbols.push_back(symbol);
                ++covered;
            }
        }
    }
    delta_offset = offset;

    // Second pass: assign deltas in symbol order (only received packets get
    // delta bytes; symbol 2 means a 2-byte delta).
    const int64_t reference_us = static_cast<int64_t>(reference_time) * 64'000;
    int64_t arrival_us = reference_us;
    bool have_arrival = false;
    for (size_t i = 0; i < symbols.size(); ++i) {
        TwccPacketResult result;
        result.sequence = static_cast<uint16_t>(base_sequence + i);
        const uint8_t symbol = symbols[i];
        if (symbol == 1 || symbol == 2) {
            int32_t delta_units;
            if (symbol == 1) {
                if (delta_offset + 1 > packet_size) {
                    return false;
                }
                delta_units = static_cast<int8_t>(data[delta_offset]);
                delta_offset += 1;
            } else {
                if (delta_offset + 2 > packet_size) {
                    return false;
                }
                delta_units = read_s16(data + delta_offset);
                delta_offset += 2;
            }
            arrival_us =
                (have_arrival ? arrival_us : reference_us)
                + static_cast<int64_t>(delta_units) * 250;
            have_arrival = true;
            result.received = true;
            result.arrival_time_us = arrival_us;
        }
        out.packets.push_back(result);
    }
    return true;
}

void TwccFeedbackGenerator::on_packet(uint16_t twcc_sequence, int64_t arrival_time_us) {
    if (has_emitted_) {
        // Ignore sequences at or behind the last emitted base: they can no
        // longer be reported (already covered) or are duplicates.
        const int16_t ahead =
            static_cast<int16_t>(twcc_sequence - last_emitted_sequence_);
        if (ahead <= 0) {
            return;
        }
    }
    pending_[twcc_sequence] = Arrival{arrival_time_us};
}

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
        // If nothing pending falls in (last_emitted, ...] we would have
        // returned above; but stale entries behind base are unreportable.
        for (auto it = pending_.begin(); it != pending_.end();) {
            if (static_cast<int16_t>(it->first - base) < 0) {
                it = pending_.erase(it);
            } else {
                ++it;
            }
        }
        if (pending_.empty()) {
            return {};
        }
    } else {
        base = pending_.begin()->first;
        for (const auto& [sequence, _] : pending_) {
            if (static_cast<int16_t>(sequence - base) < 0) {
                base = sequence;
            }
        }
    }

    // Cover [base, max seen] contiguously; unseen packets inside the range
    // are reported as not-received.
    uint16_t newest = base;
    for (const auto& [sequence, _] : pending_) {
        if (static_cast<int16_t>(sequence - newest) > 0) {
            newest = sequence;
        }
    }
    const size_t count = static_cast<uint16_t>(newest - base) + 1;
    if (count == 0 || count > 0x2000) {
        // Defensive: a wrap anomaly must not produce a giant report.
        pending_.clear();
        return {};
    }

    // Cap one report at the trailing 64 packets; older pending packets fall
    // behind the next report's base and age out (they only matter for loss
    // accounting at 50 ms cadence, where ~42 packets arrive per report).
    size_t emit_start = 0;
    if (count > 64) {
        emit_start = count - 64;
    }
    const uint16_t emit_base = static_cast<uint16_t>(base + emit_start);
    const size_t emit_count = count - emit_start;

    // Reference time = arrival of the first received packet in the emitted
    // window, so its delta is 0 and neighbor deltas stay in small-delta
    // range. (RFC 8888: first delta is relative to the reference time.)
    int64_t reference_us = pending_.at(newest).at_us;
    for (size_t j = 0; j < emit_count; ++j) {
        const auto it =
            pending_.find(static_cast<uint16_t>(emit_base + j));
        if (it != pending_.end()) {
            reference_us = it->second.at_us;
            break;
        }
    }
    const uint32_t reference_units = static_cast<uint32_t>(
        (reference_us / 64'000) & 0x00ff'ffff);

    // Build chunk list: run-length chunks for uniform spans of >= 8, 1-bit
    // status vector chunks otherwise.
    std::vector<uint8_t> chunks;
    std::vector<uint8_t> deltas;

    size_t i = 0;
    while (i < emit_count) {
        const bool received =
            pending_.count(static_cast<uint16_t>(emit_base + i)) != 0;
        size_t run = 1;
        while (i + run < emit_count
               && (pending_.count(static_cast<uint16_t>(emit_base + i + run)) != 0)
                      == received
               && run < 0x3fff) {
            ++run;
        }
        if (run >= 8) {
            const uint16_t chunk = static_cast<uint16_t>(
                (received ? 0x4000 : 0x0000) | run);
            chunks.push_back(static_cast<uint8_t>(chunk >> 8));
            chunks.push_back(static_cast<uint8_t>(chunk & 0xff));
            i += run;
            continue;
        }
        const size_t group = std::min<size_t>(14, emit_count - i);
        uint16_t chunk = 0x8000;
        for (size_t j = 0; j < group; ++j) {
            const bool r = pending_.count(
                               static_cast<uint16_t>(emit_base + i + j))
                != 0;
            if (r) {
                chunk |= static_cast<uint16_t>(1 << (13 - j));
            }
        }
        chunks.push_back(static_cast<uint8_t>(chunk >> 8));
        chunks.push_back(static_cast<uint8_t>(chunk & 0xff));
        i += group;
    }

    // Deltas in sequence order: first received packet relative to the
    // reference time, subsequent ones relative to the previous received.
    bool have_previous = false;
    int64_t previous_us = 0;
    for (size_t j = 0; j < emit_count; ++j) {
        const uint16_t sequence = static_cast<uint16_t>(emit_base + j);
        const auto it = pending_.find(sequence);
        if (it == pending_.end()) {
            continue;
        }
        const int64_t base_us = have_previous ? previous_us : reference_us;
        int64_t delta_units = (it->second.at_us - base_us) / 250;
        delta_units = std::clamp<int64_t>(delta_units, -128, 127);
        deltas.push_back(static_cast<uint8_t>(
            static_cast<int8_t>(delta_units)));
        previous_us = it->second.at_us;
        have_previous = true;
    }

    // Assemble: header(4) + ssrcs(8) + base(2) + count(2) + ref(3) + fbcount(1)
    // = 20 bytes, then chunks + deltas, padded to 4 bytes.
    const size_t body = 20 + chunks.size() + deltas.size();
    const size_t padded = (body + 3) & ~size_t{3};
    std::vector<uint8_t> packet(padded, 0);
    packet[0] = static_cast<uint8_t>(0x80 | kTwccFeedbackFmt);
    packet[1] = kRtcpRtpfb;
    write_u16(packet.data() + 2, static_cast<uint16_t>(padded / 4 - 1));
    write_u32(packet.data() + 4, sender_ssrc);
    write_u32(packet.data() + 8, media_ssrc);
    write_u16(packet.data() + 12, emit_base);
    write_u16(packet.data() + 14, static_cast<uint16_t>(emit_count));
    write_u24(packet.data() + 16, reference_units);
    packet[19] = feedback_packet_count_++;
    std::copy(chunks.begin(), chunks.end(), packet.begin() + 20);
    std::copy(
        deltas.begin(),
        deltas.end(),
        packet.begin() + 20 + static_cast<ptrdiff_t>(chunks.size()));

    for (size_t j = 0; j < emit_count; ++j) {
        pending_.erase(static_cast<uint16_t>(emit_base + j));
    }
    last_emitted_sequence_ = static_cast<uint16_t>(emit_base + emit_count - 1);
    has_emitted_ = true;
    return packet;
}

GccEstimator::GccEstimator(Config config, uint64_t initial_bps)
    : config_(config),
      target_bps_(std::clamp(initial_bps, config.min_bps, config.max_bps)) {}

void GccEstimator::on_packet(
    uint16_t /*sequence*/,
    bool received,
    int64_t send_time_us,
    int64_t arrival_time_us
) {
    ++feedback_packets_seen_;
    if (!received) {
        ++feedback_packets_lost_;
    }
    const double instant_loss = received ? 0.0 : 1.0;
    smoothed_loss_ += (instant_loss - smoothed_loss_) * 0.05;

    if (!received || send_time_us < 0) {
        return;
    }

    if (has_open_group_
        && send_time_us - open_group_.last_send_us > kGroupSendWindowUs) {
        close_group(arrival_time_us);
    }
    if (!has_open_group_) {
        open_group_ = Group{};
        open_group_.first_send_us = send_time_us;
        has_open_group_ = true;
    }
    open_group_.last_send_us = send_time_us;
    open_group_.arrival_sum_us += arrival_time_us;
    open_group_.received += 1;
    open_group_.packets += 1;
}

void GccEstimator::close_group(int64_t now_us) {
    const Group group = open_group_;
    open_group_ = Group{};
    has_open_group_ = false;
    if (group.received == 0) {
        return;
    }

    const int64_t send_mean_us =
        (group.first_send_us + group.last_send_us) / 2;
    const int64_t arrival_mean_us =
        group.arrival_sum_us / static_cast<int64_t>(group.received);

    if (has_prev_group_) {
        const double delta_ms =
            static_cast<double>(arrival_mean_us - prev_arrival_mean_us_)
            / 1000.0
            - static_cast<double>(send_mean_us - prev_send_mean_us_) / 1000.0;
        accumulated_delay_ms_ += delta_ms;
        trendline_.push_back(accumulated_delay_ms_);
        if (trendline_.size() > kTrendlineWindow) {
            trendline_.pop_front();
        }
        trend_ms_ = trendline_slope_ms() * static_cast<double>(trendline_.size());

        if (trend_ms_ > config_.overuse_threshold_ms) {
            if (overuse_since_us_ == 0) {
                overuse_since_us_ = now_us;
            }
            if (now_us - overuse_since_us_
                > config_.overuse_time_ms * 1'000) {
                // Overuse persists: multiplicative decrease, then re-arm so
                // continued buildup keeps backing off every interval.
                target_bps_ = std::clamp(
                    static_cast<uint64_t>(
                        static_cast<double>(target_bps_) * 0.85),
                    config_.min_bps,
                    config_.max_bps);
                overusing_ = true;
                overuse_since_us_ = now_us;
                last_decrease_us_ = now_us;
            }
        } else if (trend_ms_ < -config_.overuse_threshold_ms) {
            overusing_ = false;
            overuse_since_us_ = 0;
        } else {
            overuse_since_us_ = 0;
            if (!overusing_
                && trendline_.size() >= kTrendlineWindow / 2) {
                // Additive-ish increase: ~5% + 100 kbps per closed group,
                // gated on a half-full trendline window so the estimator
                // does not ramp on thin data.
                const uint64_t step = std::max<uint64_t>(
                    target_bps_ / 20,
                    100'000);
                target_bps_ = std::clamp(
                    target_bps_ + step,
                    config_.min_bps,
                    config_.max_bps);
                last_increase_us_ = now_us;
            }
        }

        // Loss-based cap on top of the delay signal.
        if (smoothed_loss_ > config_.loss_high_water) {
            const double excess = smoothed_loss_ - config_.loss_high_water;
            target_bps_ = std::clamp(
                static_cast<uint64_t>(
                    static_cast<double>(target_bps_) * (1.0 - 0.5 * excess)),
                config_.min_bps,
                config_.max_bps);
        }
    }

    prev_arrival_mean_us_ = arrival_mean_us;
    prev_send_mean_us_ = send_mean_us;
    has_prev_group_ = true;
}

double GccEstimator::trendline_slope_ms() const {
    const size_t n = trendline_.size();
    if (n < 2) {
        return 0.0;
    }
    double sum_x = 0, sum_y = 0, sum_xy = 0, sum_xx = 0;
    for (size_t i = 0; i < n; ++i) {
        const double x = static_cast<double>(i);
        const double y = trendline_[i];
        sum_x += x;
        sum_y += y;
        sum_xy += x * y;
        sum_xx += x * x;
    }
    const double denom = static_cast<double>(n) * sum_xx - sum_x * sum_x;
    if (denom == 0.0) {
        return 0.0;
    }
    return (static_cast<double>(n) * sum_xy - sum_x * sum_y) / denom;
}

} // namespace mello::transport
