#include "rtp_h264_receiver.hpp"

#include <algorithm>
#include <iterator>
#include <limits>
#include <map>
#include <set>
#include <utility>

namespace mello::transport {
namespace {

constexpr uint8_t kNalTypeMask = 0x1f;
constexpr uint8_t kStapA = 24;
constexpr uint8_t kFuA = 28;
constexpr uint8_t kIdr = 5;
constexpr uint8_t kSps = 7;
constexpr uint8_t kPps = 8;
constexpr uint8_t kForbiddenZeroBit = 0x80;
constexpr uint8_t kFuStartBit = 0x80;
constexpr uint8_t kFuEndBit = 0x40;
constexpr uint8_t kFuReservedBit = 0x20;
constexpr size_t kMaxAnnexBBytes = 2 * 1024 * 1024;

uint16_t read_u16_be(const uint8_t* data) {
    return static_cast<uint16_t>(
        (static_cast<uint16_t>(data[0]) << 8) |
        static_cast<uint16_t>(data[1]));
}

uint32_t read_u32_be(const uint8_t* data) {
    return (static_cast<uint32_t>(data[0]) << 24) |
           (static_cast<uint32_t>(data[1]) << 16) |
           (static_cast<uint32_t>(data[2]) << 8) |
           static_cast<uint32_t>(data[3]);
}

struct ParsedRtpPacket {
    uint8_t payload_type = 0;
    bool marker = false;
    uint16_t sequence = 0;
    uint32_t timestamp = 0;
    uint32_t ssrc = 0;
    const uint8_t* payload = nullptr;
    size_t payload_size = 0;
};

bool parse_rtp_packet(const uint8_t* data,
                      size_t size,
                      ParsedRtpPacket& packet) {
    if (data == nullptr || size < 12 || (data[0] >> 6) != 2) {
        return false;
    }

    const bool has_padding = (data[0] & 0x20) != 0;
    const bool has_extension = (data[0] & 0x10) != 0;
    const size_t csrc_count = data[0] & 0x0f;
    size_t header_size = 12 + csrc_count * 4;
    if (header_size > size) {
        return false;
    }

    if (has_extension) {
        if (size - header_size < 4) {
            return false;
        }
        const size_t extension_words = read_u16_be(data + header_size + 2);
        if (extension_words > (size - header_size - 4) / 4) {
            return false;
        }
        header_size += 4 + extension_words * 4;
    }

    size_t payload_end = size;
    if (has_padding) {
        const size_t padding_size = data[size - 1];
        if (padding_size == 0 || padding_size > payload_end - header_size) {
            return false;
        }
        payload_end -= padding_size;
    }
    if (payload_end <= header_size) {
        return false;
    }

    packet.payload_type = data[1] & 0x7f;
    packet.marker = (data[1] & 0x80) != 0;
    packet.sequence = read_u16_be(data + 2);
    packet.timestamp = read_u32_be(data + 4);
    packet.ssrc = read_u32_be(data + 8);
    packet.payload = data + header_size;
    packet.payload_size = payload_end - header_size;
    return true;
}

bool valid_single_nal_header(uint8_t header) {
    const uint8_t type = header & kNalTypeMask;
    return (header & kForbiddenZeroBit) == 0 && type >= 1 && type <= 23;
}

bool validate_h264_payload(const uint8_t* payload, size_t size) {
    if (payload == nullptr || size == 0 ||
        (payload[0] & kForbiddenZeroBit) != 0) {
        return false;
    }

    const uint8_t type = payload[0] & kNalTypeMask;
    if (type >= 1 && type <= 23) {
        return true;
    }

    if (type == kStapA) {
        size_t offset = 1;
        size_t nal_count = 0;
        while (offset < size) {
            if (size - offset < 2) {
                return false;
            }
            const size_t nal_size = read_u16_be(payload + offset);
            offset += 2;
            if (nal_size == 0 || nal_size > size - offset ||
                !valid_single_nal_header(payload[offset])) {
                return false;
            }
            offset += nal_size;
            ++nal_count;
        }
        return nal_count != 0 && offset == size;
    }

    if (type == kFuA) {
        if (size < 3) {
            return false;
        }
        const uint8_t fu_header = payload[1];
        const uint8_t original_type = fu_header & kNalTypeMask;
        const bool starts = (fu_header & kFuStartBit) != 0;
        const bool ends = (fu_header & kFuEndBit) != 0;
        return (fu_header & kFuReservedBit) == 0 &&
               original_type >= 1 && original_type <= 23 &&
               !(starts && ends);
    }

    return false;
}

} // namespace

class RtpH264Receiver::Impl {
public:
    Impl(Callbacks callbacks, const Config& config)
        : callbacks_(std::move(callbacks)),
          payload_type_(config.payload_type & 0x7f),
          pli_cooldown_(std::max(config.pli_cooldown,
                                 std::chrono::milliseconds::zero())) {}

    void on_rtp_packet(const uint8_t* data, size_t size, TimePoint now) {
        ++stats_.packets;
        stats_.bytes_received += static_cast<uint64_t>(size);
        if (!accept_time(now)) {
            return;
        }
        process_time(now);

        ParsedRtpPacket parsed;
        if (!parse_rtp_packet(data, size, parsed)) {
            ++stats_.invalid_rtp_packets;
            return;
        }
        if (parsed.payload_type != payload_type_) {
            ++stats_.wrong_payload_type_packets;
            return;
        }
        if (stats_.has_ssrc && parsed.ssrc != stats_.ssrc) {
            ++stats_.wrong_ssrc_packets;
            return;
        }

        const bool valid_h264 =
            parsed.payload_size <= kMaxBufferedBytes &&
            validate_h264_payload(parsed.payload, parsed.payload_size);
        if (!stats_.has_ssrc && !valid_h264) {
            ++stats_.invalid_h264_packets;
            return;
        }

        const int64_t extended_sequence = extend_sequence(parsed.sequence);
        if (seen_sequences_.count(extended_sequence) != 0) {
            ++stats_.duplicates;
            return;
        }
        if (have_release_floor_ && extended_sequence <= release_floor_) {
            ++stats_.late_packets;
            remember_sequence(extended_sequence);
            return;
        }

        observe_sequence(extended_sequence, now);

        if (!valid_h264) {
            ++stats_.invalid_h264_packets;
            mark_missing(extended_sequence, now);
            process_nacks(now);
            return;
        }

        if (!stats_.has_ssrc) {
            stats_.has_ssrc = true;
            stats_.ssrc = parsed.ssrc;
            base_sequence_ = extended_sequence;
        }
        if (missing_.erase(extended_sequence) != 0) {
            ++stats_.repaired_packets;
        }
        remember_sequence(extended_sequence);
        store_packet(parsed, extended_sequence, now);
        ++stats_.accepted_packets;
        stats_.accepted_bytes += static_cast<uint64_t>(size);
        update_interarrival_jitter(parsed.timestamp, now);

        process_nacks(now);
        process_access_units(now);
    }

    void tick(TimePoint now) {
        if (!accept_time(now)) {
            return;
        }
        process_time(now);
    }

    Stats stats() const {
        Stats result = stats_;
        result.buffered_access_units = access_units_.size();
        result.buffered_packets = buffered_packets_;
        result.buffered_bytes = buffered_bytes_;
        if (have_highest_sequence_) {
            result.extended_highest_sequence =
                static_cast<uint64_t>(highest_sequence_);
            const int64_t expected =
                highest_sequence_ - base_sequence_ + 1;
            result.cumulative_loss =
                expected -
                static_cast<int64_t>(stats_.accepted_packets);
        }
        result.gated = gated_;
        return result;
    }

    bool gated() const {
        return gated_;
    }

private:
    struct Packet {
        std::vector<uint8_t> payload;
    };

    struct AccessUnit {
        uint32_t timestamp = 0;
        std::map<int64_t, Packet> packets;
        TimePoint first_arrival{};
        bool first_observed_was_marker = false;
        bool has_marker = false;
        int64_t marker_sequence = 0;
        bool structurally_invalid = false;
    };

    struct MissingPacket {
        TimePoint first_detected{};
        TimePoint last_nack{};
        size_t attempts = 0;
        size_t newer_packets = 0;
    };

    struct ReconstructedAccessUnit {
        std::vector<uint8_t> bytes;
        bool has_idr = false;
        bool has_sps = false;
        bool has_pps = false;
    };

    bool accept_time(TimePoint now) {
        if (have_last_time_ && now < last_time_) {
            ++stats_.backwards_time_inputs;
            return false;
        }
        have_last_time_ = true;
        last_time_ = now;
        return true;
    }

    void process_time(TimePoint now) {
        process_nacks(now);
        process_access_units(now);
    }

    void update_interarrival_jitter(uint32_t timestamp, TimePoint now) {
        if (have_previous_arrival_) {
            const double arrival_delta =
                std::chrono::duration<double>(now - previous_arrival_).count() *
                90000.0;
            const uint32_t wrapped_timestamp_delta =
                timestamp - previous_rtp_timestamp_;
            int64_t timestamp_delta = wrapped_timestamp_delta;
            if (wrapped_timestamp_delta >
                static_cast<uint32_t>(std::numeric_limits<int32_t>::max())) {
                timestamp_delta -= (int64_t{1} << 32);
            }
            double deviation =
                arrival_delta - static_cast<double>(timestamp_delta);
            if (deviation < 0.0) {
                deviation = -deviation;
            }
            jitter_ += (deviation - jitter_) / 16.0;
            stats_.interarrival_jitter =
                static_cast<uint32_t>(jitter_);
        }
        have_previous_arrival_ = true;
        previous_arrival_ = now;
        previous_rtp_timestamp_ = timestamp;
    }

    int64_t extend_sequence(uint16_t sequence) const {
        if (!have_highest_sequence_) {
            return sequence;
        }

        const int64_t cycle_base = highest_sequence_ & ~int64_t{0xffff};
        int64_t candidate = cycle_base + sequence;
        const int64_t delta = candidate - highest_sequence_;
        if (delta > 32768) {
            candidate -= 65536;
        } else if (delta < -32768) {
            candidate += 65536;
        }
        return candidate;
    }

    void observe_sequence(int64_t sequence, TimePoint now) {
        if (!have_highest_sequence_) {
            highest_sequence_ = sequence;
            have_highest_sequence_ = true;
            return;
        }
        if (sequence <= highest_sequence_) {
            return;
        }

        for (auto& entry : missing_) {
            if (entry.first < sequence &&
                entry.second.newer_packets < kNewerPacketGrace) {
                ++entry.second.newer_packets;
            }
        }

        const int64_t gap = sequence - highest_sequence_ - 1;
        if (gap > static_cast<int64_t>(kMaxPackets) ||
            static_cast<size_t>(gap) >
                kMaxPackets - std::min(kMaxPackets, missing_.size())) {
            ++stats_.sequence_discontinuities;
            drop_all_access_units(now, true);
            missing_.clear();
            set_release_floor(sequence - 1);
            next_access_unit_start_ = sequence;
            have_next_access_unit_start_ = true;
        } else {
            for (int64_t missing_sequence = highest_sequence_ + 1;
                 missing_sequence < sequence;
                 ++missing_sequence) {
                MissingPacket missing;
                missing.first_detected = now;
                missing.newer_packets = 1;
                if (missing_.emplace(missing_sequence, missing).second) {
                    ++stats_.missing_sequences_detected;
                }
            }
        }

        highest_sequence_ = sequence;
        const int64_t prune_before = highest_sequence_ - 1024;
        seen_sequences_.erase(seen_sequences_.begin(),
                              seen_sequences_.lower_bound(prune_before));
    }

    void mark_missing(int64_t sequence, TimePoint now) {
        auto existing = missing_.find(sequence);
        if (existing != missing_.end()) {
            return;
        }
        if (missing_.size() >= kMaxPackets) {
            ++stats_.sequence_discontinuities;
            const bool transitioned = enter_gate(now);
            if (!transitioned) {
                request_pli(now);
            }
            return;
        }
        MissingPacket missing;
        missing.first_detected = now;
        if (missing_.emplace(sequence, missing).second) {
            ++stats_.missing_sequences_detected;
        }
    }

    void remember_sequence(int64_t sequence) {
        seen_sequences_.insert(sequence);
    }

    void process_nacks(TimePoint now) {
        std::vector<uint16_t> due;
        due.reserve(missing_.size());
        for (auto& entry : missing_) {
            MissingPacket& missing = entry.second;
            bool should_send = false;
            if (missing.attempts == 0) {
                should_send =
                    now - missing.first_detected >= kReorderGrace ||
                    missing.newer_packets >= kNewerPacketGrace;
            } else if (missing.attempts < kMaxNackAttempts) {
                should_send = now - missing.last_nack >= kNackRepeat;
            }

            if (should_send) {
                due.push_back(static_cast<uint16_t>(entry.first & 0xffff));
                missing.last_nack = now;
                ++missing.attempts;
            }
        }

        if (!due.empty()) {
            stats_.nacks += due.size();
            ++stats_.nack_callbacks;
            if (callbacks_.on_nack) {
                callbacks_.on_nack(due);
            }
        }
    }

    size_t find_access_unit(uint32_t timestamp) const {
        for (size_t i = 0; i < access_units_.size(); ++i) {
            if (access_units_[i].timestamp == timestamp) {
                return i;
            }
        }
        return access_units_.size();
    }

    size_t oldest_access_unit() const {
        size_t oldest = 0;
        for (size_t i = 1; i < access_units_.size(); ++i) {
            if (access_units_[i].packets.begin()->first <
                access_units_[oldest].packets.begin()->first) {
                oldest = i;
            }
        }
        return oldest;
    }

    void store_packet(const ParsedRtpPacket& parsed,
                      int64_t extended_sequence,
                      TimePoint now) {
        size_t index = find_access_unit(parsed.timestamp);
        while (!access_units_.empty() &&
               (buffered_packets_ >= kMaxPackets ||
                buffered_bytes_ >
                    kMaxBufferedBytes - parsed.payload_size)) {
            drop_access_unit(oldest_access_unit(), now, true, true);
            index = find_access_unit(parsed.timestamp);
        }

        if (index == access_units_.size()) {
            if (access_units_.size() >= kMaxAccessUnits) {
                drop_access_unit(oldest_access_unit(), now, true, true);
            }

            AccessUnit access_unit;
            access_unit.timestamp = parsed.timestamp;
            access_unit.first_arrival = now;
            access_unit.first_observed_was_marker = parsed.marker;
            access_units_.push_back(std::move(access_unit));
            index = access_units_.size() - 1;
        }

        AccessUnit& access_unit = access_units_[index];
        Packet packet;
        packet.payload.assign(parsed.payload,
                              parsed.payload + parsed.payload_size);
        access_unit.packets.emplace(extended_sequence, std::move(packet));
        ++buffered_packets_;
        buffered_bytes_ += parsed.payload_size;

        if (parsed.marker) {
            if (access_unit.has_marker &&
                access_unit.marker_sequence != extended_sequence) {
                access_unit.structurally_invalid = true;
            }
            access_unit.has_marker = true;
            access_unit.marker_sequence = extended_sequence;
        }

        update_peaks();
    }

    void update_peaks() {
        stats_.peak_buffered_access_units =
            std::max(stats_.peak_buffered_access_units, access_units_.size());
        stats_.peak_buffered_packets =
            std::max(stats_.peak_buffered_packets, buffered_packets_);
        stats_.peak_buffered_bytes =
            std::max(stats_.peak_buffered_bytes, buffered_bytes_);
    }

    bool next_timestamp_boundary(size_t index,
                                 int64_t start,
                                 int64_t& end) const {
        bool found = false;
        int64_t next_start = std::numeric_limits<int64_t>::max();
        for (size_t i = 0; i < access_units_.size(); ++i) {
            if (i == index || access_units_[i].packets.empty()) {
                continue;
            }
            const int64_t candidate =
                access_units_[i].packets.begin()->first;
            if (candidate > start && candidate < next_start) {
                next_start = candidate;
                found = true;
            }
        }
        if (found) {
            end = next_start - 1;
        }
        return found;
    }

    bool has_other_timestamp_in_range(size_t index,
                                      int64_t start,
                                      int64_t end) const {
        for (size_t i = 0; i < access_units_.size(); ++i) {
            if (i == index) {
                continue;
            }
            auto it = access_units_[i].packets.lower_bound(start);
            if (it != access_units_[i].packets.end() && it->first <= end) {
                return true;
            }
        }
        return false;
    }

    size_t count_seen_after(int64_t sequence) const {
        const auto begin = seen_sequences_.upper_bound(sequence);
        return static_cast<size_t>(std::distance(begin,
                                                  seen_sequences_.end()));
    }

    bool append_nal(ReconstructedAccessUnit& output,
                    const uint8_t* nal,
                    size_t nal_size) const {
        constexpr uint8_t start_code[] = {0, 0, 0, 1};
        if (nal_size > kMaxAnnexBBytes - sizeof(start_code) ||
            output.bytes.size() >
                kMaxAnnexBBytes - sizeof(start_code) - nal_size) {
            return false;
        }
        output.bytes.insert(output.bytes.end(),
                            std::begin(start_code),
                            std::end(start_code));
        output.bytes.insert(output.bytes.end(), nal, nal + nal_size);

        const uint8_t type = nal[0] & kNalTypeMask;
        output.has_idr = output.has_idr || type == kIdr;
        output.has_sps = output.has_sps || type == kSps;
        output.has_pps = output.has_pps || type == kPps;
        return true;
    }

    bool reconstruct(const AccessUnit& access_unit,
                     int64_t start,
                     int64_t end,
                     ReconstructedAccessUnit& output) const {
        bool fu_open = false;
        uint8_t fu_nal_header = 0;
        std::vector<uint8_t> fu_nal;

        for (int64_t sequence = start; sequence <= end; ++sequence) {
            const Packet& packet = access_unit.packets.at(sequence);
            const uint8_t* payload = packet.payload.data();
            const size_t size = packet.payload.size();
            const uint8_t type = payload[0] & kNalTypeMask;

            if (type >= 1 && type <= 23) {
                if (fu_open || !append_nal(output, payload, size)) {
                    return false;
                }
                continue;
            }

            if (type == kStapA) {
                if (fu_open) {
                    return false;
                }
                size_t offset = 1;
                while (offset < size) {
                    const size_t nal_size = read_u16_be(payload + offset);
                    offset += 2;
                    if (!append_nal(output, payload + offset, nal_size)) {
                        return false;
                    }
                    offset += nal_size;
                }
                continue;
            }

            if (type != kFuA) {
                return false;
            }

            const uint8_t fu_header = payload[1];
            const bool starts = (fu_header & kFuStartBit) != 0;
            const bool ends = (fu_header & kFuEndBit) != 0;
            const uint8_t reconstructed_header =
                static_cast<uint8_t>((payload[0] & 0xe0) |
                                     (fu_header & kNalTypeMask));

            if (starts) {
                if (fu_open) {
                    return false;
                }
                fu_open = true;
                fu_nal_header = reconstructed_header;
                fu_nal.clear();
                fu_nal.push_back(reconstructed_header);
            } else if (!fu_open || reconstructed_header != fu_nal_header) {
                return false;
            }

            if (fu_nal.size() > kMaxBufferedBytes - (size - 2)) {
                return false;
            }
            fu_nal.insert(fu_nal.end(), payload + 2, payload + size);
            if (ends) {
                if (!append_nal(output, fu_nal.data(), fu_nal.size())) {
                    return false;
                }
                fu_open = false;
                fu_nal.clear();
            }
        }

        return !fu_open && !output.bytes.empty();
    }

    void process_access_units(TimePoint now) {
        while (!access_units_.empty()) {
            const size_t index = oldest_access_unit();
            AccessUnit& access_unit = access_units_[index];
            const int64_t minimum_sequence =
                access_unit.packets.begin()->first;
            const int64_t maximum_sequence =
                access_unit.packets.rbegin()->first;
            const int64_t start =
                have_next_access_unit_start_
                    ? next_access_unit_start_
                    : minimum_sequence;

            bool boundary_known = false;
            int64_t end = 0;
            if (access_unit.has_marker) {
                boundary_known = true;
                end = access_unit.marker_sequence;
                if (end < maximum_sequence) {
                    access_unit.structurally_invalid = true;
                }
            } else {
                boundary_known =
                    next_timestamp_boundary(index, start, end);
            }

            if (access_unit.structurally_invalid ||
                minimum_sequence < start ||
                (boundary_known &&
                 has_other_timestamp_in_range(index, start, end))) {
                ++stats_.invalid_h264_packets;
                drop_access_unit(index, now, false, false);
                continue;
            }

            const bool expired =
                now - access_unit.first_arrival >= kAccessUnitDeadline;
            if (!boundary_known) {
                if (expired) {
                    drop_access_unit(index, now, false, true);
                    continue;
                }
                break;
            }

            if (!have_next_access_unit_start_ &&
                access_unit.first_observed_was_marker &&
                now - access_unit.first_arrival < kReorderGrace &&
                count_seen_after(access_unit.marker_sequence) <
                    kNewerPacketGrace) {
                break;
            }

            const int64_t span = end - start + 1;
            bool all_packets_present =
                span > 0 &&
                span <= static_cast<int64_t>(kMaxPackets);
            if (all_packets_present) {
                for (int64_t sequence = start;
                     sequence <= end;
                     ++sequence) {
                    if (access_unit.packets.count(sequence) == 0) {
                        all_packets_present = false;
                        break;
                    }
                }
            }

            if (!all_packets_present) {
                if (expired) {
                    drop_access_unit(index, now, false, true);
                    continue;
                }
                break;
            }

            ReconstructedAccessUnit reconstructed;
            if (!reconstruct(access_unit, start, end, reconstructed)) {
                ++stats_.invalid_h264_packets;
                drop_access_unit(index, now, false, false);
                continue;
            }

            const uint32_t timestamp = access_unit.timestamp;
            ++stats_.complete_access_units;
            release_access_unit(index, end);

            const bool opens_gate =
                reconstructed.has_idr &&
                reconstructed.has_sps &&
                reconstructed.has_pps;
            bool emit = !gated_;
            if (gated_) {
                if (opens_gate) {
                    gated_ = false;
                    ++stats_.gate_exits;
                    emit = true;
                } else {
                    ++stats_.gate_dropped_access_units;
                    request_pli(now);
                }
            }

            if (emit) {
                ++stats_.emitted_access_units;
                if (callbacks_.on_access_unit) {
                    callbacks_.on_access_unit(reconstructed.bytes,
                                              reconstructed.has_idr,
                                              timestamp);
                }
            }
        }
    }

    void release_access_unit(size_t index, int64_t end) {
        const AccessUnit& access_unit = access_units_[index];
        buffered_packets_ -= access_unit.packets.size();
        for (const auto& entry : access_unit.packets) {
            buffered_bytes_ -= entry.second.payload.size();
        }
        access_units_.erase(access_units_.begin() +
                            static_cast<std::ptrdiff_t>(index));

        set_release_floor(end);
        next_access_unit_start_ = end + 1;
        have_next_access_unit_start_ = true;
        erase_missing_through(end);
    }

    void drop_access_unit(size_t index,
                          TimePoint now,
                          bool eviction,
                          bool request_even_if_gated) {
        const AccessUnit& access_unit = access_units_[index];
        int64_t start = access_unit.packets.begin()->first;
        if (have_next_access_unit_start_) {
            start = next_access_unit_start_;
        }

        int64_t end = access_unit.packets.rbegin()->first;
        if (access_unit.has_marker) {
            end = std::max(end, access_unit.marker_sequence);
        } else {
            int64_t timestamp_end = 0;
            if (next_timestamp_boundary(index, start, timestamp_end)) {
                end = std::max(end, timestamp_end);
            }
        }

        ++stats_.incomplete_access_units;
        if (eviction) {
            ++stats_.buffer_evictions;
        }
        release_access_unit(index, end);

        const bool transitioned = enter_gate(now);
        if (request_even_if_gated && !transitioned) {
            request_pli(now);
        }
    }

    void drop_all_access_units(TimePoint now,
                               bool request_even_if_gated) {
        bool dropped_any = false;
        while (!access_units_.empty()) {
            drop_access_unit(oldest_access_unit(),
                             now,
                             true,
                             request_even_if_gated && !dropped_any);
            dropped_any = true;
        }
        if (!dropped_any) {
            const bool transitioned = enter_gate(now);
            if (request_even_if_gated && !transitioned) {
                request_pli(now);
            }
        }
    }

    void set_release_floor(int64_t sequence) {
        if (!have_release_floor_ || sequence > release_floor_) {
            release_floor_ = sequence;
            have_release_floor_ = true;
        }
    }

    void erase_missing_through(int64_t sequence) {
        missing_.erase(missing_.begin(),
                       missing_.upper_bound(sequence));
    }

    bool enter_gate(TimePoint now) {
        if (gated_) {
            return false;
        }
        gated_ = true;
        ++stats_.gate_entries;
        request_pli(now);
        return true;
    }

    void request_pli(TimePoint now) {
        if (have_last_pli_ && now - last_pli_ < pli_cooldown_) {
            return;
        }
        have_last_pli_ = true;
        last_pli_ = now;
        ++stats_.pli_requests;
        if (callbacks_.on_pli) {
            callbacks_.on_pli();
        }
    }

    Callbacks callbacks_;
    uint8_t payload_type_ = 96;
    std::chrono::milliseconds pli_cooldown_{1000};
    Stats stats_;

    std::vector<AccessUnit> access_units_;
    std::map<int64_t, MissingPacket> missing_;
    std::set<int64_t> seen_sequences_;
    size_t buffered_packets_ = 0;
    size_t buffered_bytes_ = 0;

    bool have_highest_sequence_ = false;
    int64_t highest_sequence_ = 0;
    int64_t base_sequence_ = 0;
    bool have_release_floor_ = false;
    int64_t release_floor_ = 0;
    bool have_next_access_unit_start_ = false;
    int64_t next_access_unit_start_ = 0;

    bool gated_ = true;
    bool have_last_pli_ = false;
    TimePoint last_pli_{};
    bool have_last_time_ = false;
    TimePoint last_time_{};
    bool have_previous_arrival_ = false;
    TimePoint previous_arrival_{};
    uint32_t previous_rtp_timestamp_ = 0;
    double jitter_ = 0.0;
};

RtpH264Receiver::RtpH264Receiver()
    : RtpH264Receiver(Callbacks{}, Config{}) {}

RtpH264Receiver::RtpH264Receiver(Callbacks callbacks)
    : RtpH264Receiver(std::move(callbacks), Config{}) {}

RtpH264Receiver::RtpH264Receiver(Callbacks callbacks,
                                 const Config& config)
    : impl_(std::make_unique<Impl>(std::move(callbacks), config)) {}

RtpH264Receiver::~RtpH264Receiver() = default;

void RtpH264Receiver::on_rtp_packet(const uint8_t* data,
                                    size_t size,
                                    TimePoint now) {
    impl_->on_rtp_packet(data, size, now);
}

void RtpH264Receiver::tick(TimePoint now) {
    impl_->tick(now);
}

RtpH264Receiver::Stats RtpH264Receiver::stats() const {
    return impl_->stats();
}

bool RtpH264Receiver::gated() const {
    return impl_->gated();
}

} // namespace mello::transport
