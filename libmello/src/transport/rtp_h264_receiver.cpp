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
constexpr uint8_t kSei = 6;
constexpr uint8_t kStapA = 24;
constexpr uint8_t kFuA = 28;
constexpr uint8_t kIdr = 5;
constexpr uint8_t kNonIdrSlice = 1;
constexpr uint8_t kSps = 7;
constexpr uint8_t kPps = 8;
constexpr uint8_t kAud = 9;
constexpr uint8_t kForbiddenZeroBit = 0x80;
constexpr uint8_t kFuStartBit = 0x80;
constexpr uint8_t kFuEndBit = 0x40;
constexpr uint8_t kFuReservedBit = 0x20;
constexpr size_t kMaxAnnexBBytes = 4 * 1024 * 1024;
constexpr size_t kMaxPayloadBytes = 64 * 1024;

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

enum class ParseResult {
    Invalid,
    Media,
    // A valid RTP header with only padding after it (RFC 3550 5.1): a
    // pacer's padding packet. It takes a sequence number but carries no
    // video.
    Padding,
};

ParseResult parse_rtp_packet(const uint8_t* data,
                             size_t size,
                             ParsedRtpPacket& packet) {
    if (data == nullptr || size < 12 || (data[0] >> 6) != 2) {
        return ParseResult::Invalid;
    }

    const bool has_padding = (data[0] & 0x20) != 0;
    const bool has_extension = (data[0] & 0x10) != 0;
    const size_t csrc_count = data[0] & 0x0f;
    size_t header_size = 12 + csrc_count * 4;
    if (header_size > size) {
        return ParseResult::Invalid;
    }

    if (has_extension) {
        if (size - header_size < 4) {
            return ParseResult::Invalid;
        }
        const size_t extension_words = read_u16_be(data + header_size + 2);
        if (extension_words > (size - header_size - 4) / 4) {
            return ParseResult::Invalid;
        }
        header_size += 4 + extension_words * 4;
    }

    size_t payload_end = size;
    if (has_padding) {
        const size_t padding_size = data[size - 1];
        if (padding_size == 0 || padding_size > payload_end - header_size) {
            return ParseResult::Invalid;
        }
        payload_end -= padding_size;
    }

    packet.payload_type = data[1] & 0x7f;
    packet.marker = (data[1] & 0x80) != 0;
    packet.sequence = read_u16_be(data + 2);
    packet.timestamp = read_u32_be(data + 4);
    packet.ssrc = read_u32_be(data + 8);
    packet.payload = data + header_size;
    packet.payload_size = payload_end > header_size ? payload_end - header_size : 0;
    if (packet.payload_size == 0) {
        return has_padding ? ParseResult::Padding : ParseResult::Invalid;
    }
    return ParseResult::Media;
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

// One NAL (header byte first) that can only open an access unit.
bool nal_starts_access_unit(uint8_t type, const uint8_t* body, size_t body_size) {
    if (type == kAud || type == kSps || type == kSei) {
        return true;
    }
    if (type == kIdr || type == kNonIdrSlice) {
        // first_mb_in_slice is the first ue(v) of the slice header; it is 0
        // exactly when its first bit is 1.
        return body_size >= 1 && (body[0] & 0x80) != 0;
    }
    return false;
}

// A payload that begins a NAL (not the middle or end of an FU-A).
bool starts_nal(const uint8_t* payload, size_t size) {
    if (payload == nullptr || size == 0) return false;
    const uint8_t type = payload[0] & kNalTypeMask;
    if (type >= 1 && type <= 23) return true;
    if (type == kStapA) return true;
    return type == kFuA && size >= 2 && (payload[1] & kFuStartBit) != 0;
}

// The NAL type that opens a payload: the single NAL, the first NAL of a
// STAP-A, or the original type of a starting FU-A. 0 when there is none.
uint8_t first_nal_type(const uint8_t* payload, size_t size) {
    if (payload == nullptr || size == 0) return 0;
    const uint8_t type = payload[0] & kNalTypeMask;
    if (type >= 1 && type <= 23) return type;
    if (type == kStapA && size >= 4) return payload[3] & kNalTypeMask;
    if (type == kFuA && size >= 2 && (payload[1] & kFuStartBit) != 0) {
        return payload[1] & kNalTypeMask;
    }
    return 0;
}

} // namespace

bool h264_payload_starts_access_unit(const uint8_t* payload, size_t size) {
    if (payload == nullptr || size == 0) return false;
    const uint8_t type = payload[0] & kNalTypeMask;
    if (type >= 1 && type <= 23) {
        return nal_starts_access_unit(type, payload + 1, size - 1);
    }
    if (type == kStapA) {
        if (size < 4) return false;
        const size_t nal_size = read_u16_be(payload + 1);
        if (nal_size == 0 || nal_size > size - 3) return false;
        return nal_starts_access_unit(payload[3] & kNalTypeMask, payload + 4,
                                      nal_size - 1);
    }
    if (type == kFuA) {
        if (size < 3 || (payload[1] & kFuStartBit) == 0) return false;
        return nal_starts_access_unit(payload[1] & kNalTypeMask, payload + 2,
                                      size - 2);
    }
    return false;
}

class RtpH264Receiver::Impl {
public:
    Impl(Callbacks callbacks, const Config& config)
        : callbacks_(std::move(callbacks)),
          payload_type_(config.payload_type & 0x7f),
          rtt_(std::max(config.rtt, std::chrono::microseconds::zero())) {}

    void on_rtp_packet(const uint8_t* data, size_t size, TimePoint now) {
        ++stats_.packets;
        stats_.bytes_received += static_cast<uint64_t>(size);
        if (!accept_time(now)) {
            return;
        }

        ParsedRtpPacket parsed;
        const ParseResult kind = parse_rtp_packet(data, size, parsed);
        if (kind == ParseResult::Invalid) {
            ++stats_.invalid_rtp_packets;
            process_time(now);
            return;
        }
        if (parsed.payload_type != payload_type_) {
            ++stats_.wrong_payload_type_packets;
            process_time(now);
            return;
        }
        if (stats_.has_ssrc && parsed.ssrc != stats_.ssrc) {
            ++stats_.wrong_ssrc_packets;
            process_time(now);
            return;
        }

        const bool padding = kind == ParseResult::Padding;
        const bool valid_h264 =
            padding ||
            (parsed.payload_size <= kMaxPayloadBytes &&
             validate_h264_payload(parsed.payload, parsed.payload_size));
        if (!stats_.has_ssrc && (padding || !valid_h264)) {
            // The first packet chooses the SSRC; it must carry video.
            if (!valid_h264) ++stats_.invalid_h264_packets;
            process_time(now);
            return;
        }

        const int64_t sequence = extend_sequence(parsed.sequence);
        if (packets_.count(sequence) != 0) {
            ++stats_.duplicates;
            process_time(now);
            return;
        }
        if ((have_floor_ && sequence <= floor_) || lost_.count(sequence) != 0) {
            ++stats_.late_packets;
            process_time(now);
            return;
        }

        if (!stats_.has_ssrc) {
            stats_.has_ssrc = true;
            stats_.ssrc = parsed.ssrc;
            base_sequence_ = sequence;
            first_sequence_ = sequence;
        }

        const bool repaired = missing_.erase(sequence) != 0;
        if (repaired) {
            ++stats_.repaired_packets;
        }
        observe_sequence(sequence, now);
        last_any_arrival_ = now;
        ++stats_.accepted_packets;
        stats_.accepted_bytes += static_cast<uint64_t>(size);
        update_interarrival_jitter(parsed.timestamp, now);

        Packet packet;
        packet.timestamp = parsed.timestamp;
        packet.marker = parsed.marker;
        packet.arrival = now;
        packet.repaired = repaired;
        if (padding) {
            ++stats_.padding_packets;
            packet.padding = true;
            packet.emitted = true;  // holds no video
            packets_.emplace(sequence, std::move(packet));
        } else if (!valid_h264) {
            // A corrupt packet is as good as a missing one: NACK it.
            ++stats_.invalid_h264_packets;
            --stats_.accepted_packets;
            stats_.accepted_bytes -= static_cast<uint64_t>(size);
            mark_missing(sequence, now);
        } else {
            packet.payload.assign(parsed.payload,
                                  parsed.payload + parsed.payload_size);
            buffered_bytes_ += packet.payload.size();
            ++buffered_packets_;
            packets_.emplace(sequence, std::move(packet));
            unemitted_.insert(sequence);
            update_peaks();
        }

        enforce_memory_bounds(now);
        // A new packet (or a padding packet) can settle the end of the access
        // unit before it and the start of the one after it. The older one
        // goes first.
        try_complete_around(sequence - 1, now);
        try_complete_around(sequence, now);
        try_complete_around(sequence + 1, now);
        process_time(now);
    }

    void tick(TimePoint now) {
        if (!accept_time(now)) {
            return;
        }
        process_time(now);
    }

    void set_rtt(std::chrono::microseconds rtt) {
        rtt_ = std::max(rtt, std::chrono::microseconds::zero());
        // Deadlines move with the RTT: compute them again when next due.
        for (auto& entry : missing_) {
            entry.second.deadline_known = false;
        }
    }

    bool keyframe_in_progress(TimePoint now) const {
        for (const int64_t sequence : unemitted_) {
            const Packet& p = packets_.at(sequence);
            const uint8_t type = first_nal_type(p.payload.data(), p.payload.size());
            if (type != kSps && type != kIdr) continue;
            int64_t lo = 0;
            int64_t hi = 0;
            if (!group_of(sequence, lo, hi)) continue;
            // The newest packet of this access unit.
            TimePoint last = p.arrival;
            for (int64_t s = lo; s <= hi; ++s) {
                last = std::max(last, packets_.at(s).arrival);
            }
            if (now - last <= kKeyframeProgress) return true;
        }
        return false;
    }

    uint64_t accepted_packets() const { return stats_.accepted_packets; }

    Stats stats() const {
        Stats result = stats_;
        result.buffered_packets = buffered_packets_;
        result.buffered_bytes = buffered_bytes_;
        result.buffered_access_units = count_groups();
        if (have_highest_sequence_) {
            result.extended_highest_sequence =
                static_cast<uint64_t>(highest_sequence_);
            const int64_t expected = highest_sequence_ - base_sequence_ + 1;
            result.cumulative_loss =
                expected - static_cast<int64_t>(stats_.accepted_packets);
        }
        return result;
    }

private:
    struct Packet {
        uint32_t timestamp = 0;
        bool marker = false;
        bool padding = false;
        // Its access unit went out (or it is padding): only the header facts
        // stay, for the start and end of the access units next to it.
        bool emitted = false;
        bool repaired = false;
        TimePoint arrival{};
        std::vector<uint8_t> payload;
    };

    struct Missing {
        TimePoint first_detected{};
        TimePoint last_nack{};
        size_t attempts = 0;
        size_t newer_packets = 0;
        bool deadline_known = false;
        TimePoint deadline{};
    };

    struct ReconstructedAccessUnit {
        std::vector<uint8_t> bytes;
        bool has_idr = false;
        bool has_sps = false;
        bool has_pps = false;
    };

    // What is known about the packet before `sequence`, skipping padding.
    enum class Before {
        // A media packet: `prev` points at it.
        Media,
        // The packet is lost for real, older than the last key frame, or
        // before the first packet of the stream: it will never be known.
        Gone,
        // Missing, and it may still arrive.
        Unknown,
    };

    Before packet_before(int64_t sequence, std::map<int64_t, Packet>::const_iterator& prev) const {
        int64_t s = sequence - 1;
        for (;;) {
            if (s < first_sequence_ || (have_floor_ && s <= floor_) || lost_.count(s) != 0) {
                return Before::Gone;
            }
            auto it = packets_.find(s);
            if (it == packets_.end()) {
                return Before::Unknown;
            }
            if (it->second.padding) {
                --s;
                continue;
            }
            prev = it;
            return Before::Media;
        }
    }

    int64_t reference_of(int64_t first_sequence) const {
        int64_t s = first_sequence - 1;
        for (auto it = packets_.find(s); it != packets_.end() && it->second.padding;
             it = packets_.find(s)) {
            --s;
        }
        return s;
    }

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
        forget_silent(now);
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
            stats_.interarrival_jitter = static_cast<uint32_t>(jitter_);
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
        if (gap > kMaxGap) {
            // A jump (a host restart, an SFU replay of an old IDR, a long
            // outage): NACKing it is pointless. Everything before is gone.
            ++stats_.sequence_discontinuities;
            give_up_before(sequence, now, true);
        } else {
            for (int64_t missing_sequence = highest_sequence_ + 1;
                 missing_sequence < sequence;
                 ++missing_sequence) {
                Missing missing;
                missing.first_detected = now;
                missing.newer_packets = 1;
                if (missing_.emplace(missing_sequence, missing).second) {
                    ++stats_.missing_sequences_detected;
                }
            }
        }

        highest_sequence_ = sequence;
        prune_history();
    }

    void mark_missing(int64_t sequence, TimePoint now) {
        Missing missing;
        missing.first_detected = now;
        if (missing_.emplace(sequence, missing).second) {
            ++stats_.missing_sequences_detected;
        }
    }

    void prune_history() {
        const int64_t keep_from = highest_sequence_ - kHistory;
        for (auto it = packets_.begin(); it != packets_.end() && it->first < keep_from;) {
            if (!it->second.emitted) {
                // Never completed and now out of memory.
                release_payload(it->first, it->second);
                ++stats_.incomplete_access_units;
            }
            it = packets_.erase(it);
        }
        lost_.erase(lost_.begin(), lost_.lower_bound(keep_from));
        missing_.erase(missing_.begin(), missing_.lower_bound(keep_from));
    }

    void release_payload(int64_t sequence, Packet& packet) {
        if (!packet.emitted && !packet.padding) {
            buffered_bytes_ -= packet.payload.size();
            --buffered_packets_;
            unemitted_.erase(sequence);
        }
        packet.payload.clear();
        packet.payload.shrink_to_fit();
        packet.emitted = true;
    }

    void update_peaks() {
        stats_.peak_buffered_packets =
            std::max(stats_.peak_buffered_packets, buffered_packets_);
        stats_.peak_buffered_bytes =
            std::max(stats_.peak_buffered_bytes, buffered_bytes_);
        stats_.peak_buffered_access_units =
            std::max(stats_.peak_buffered_access_units, count_groups());
    }

    size_t count_groups() const {
        size_t groups = 0;
        bool in_group = false;
        uint32_t group_ts = 0;
        int64_t previous = 0;
        for (const int64_t sequence : unemitted_) {
            const Packet& packet = packets_.at(sequence);
            if (!in_group || packet.timestamp != group_ts || sequence != previous + 1) {
                ++groups;
                in_group = true;
                group_ts = packet.timestamp;
            }
            previous = sequence;
        }
        return groups;
    }

    // The bounds of the not yet emitted access unit that holds `sequence`:
    // the run of stored, not emitted media packets with its timestamp.
    bool group_of(int64_t sequence, int64_t& lo, int64_t& hi) const {
        auto it = packets_.find(sequence);
        if (it == packets_.end() || it->second.emitted || it->second.padding) {
            return false;
        }
        const uint32_t ts = it->second.timestamp;
        lo = sequence;
        hi = sequence;
        for (auto p = packets_.find(lo - 1);
             p != packets_.end() && !p->second.emitted && !p->second.padding &&
             p->second.timestamp == ts;
             p = packets_.find(lo - 1)) {
            --lo;
        }
        for (auto n = packets_.find(hi + 1);
             n != packets_.end() && !n->second.emitted && !n->second.padding &&
             n->second.timestamp == ts;
             n = packets_.find(hi + 1)) {
            ++hi;
        }
        return true;
    }

    // Is `lo` the first packet of its access unit?
    // true: yes. false: no or not known yet. `dead` set: it never will be.
    bool start_known(int64_t lo, uint32_t ts, bool& dead) const {
        dead = false;
        std::map<int64_t, Packet>::const_iterator prev;
        switch (packet_before(lo, prev)) {
        case Before::Media:
            if (prev->second.marker || prev->second.timestamp != ts) {
                return true;
            }
            // The packet before is of this access unit and already emitted
            // or given up: this run cannot be whole.
            dead = true;
            return false;
        case Before::Gone: {
            const Packet& first = packets_.at(lo);
            // The first packet of the stream: any NAL start may open the
            // first access unit (a stream starts on an access unit; a
            // viewer that joins in the middle gets a frame that references
            // nothing it has, which the timeline never releases).
            if (lo == first_sequence_ && starts_nal(first.payload.data(), first.payload.size())) {
                return true;
            }
            if (h264_payload_starts_access_unit(first.payload.data(), first.payload.size())) {
                return true;
            }
            dead = true;
            return false;
        }
        case Before::Unknown:
            return false;
        }
        return false;
    }

    // Is `hi` the last packet of its access unit?
    bool end_known(int64_t hi, uint32_t ts, bool& dead) const {
        dead = false;
        const Packet& last = packets_.at(hi);
        if (last.marker) {
            return true;
        }
        // No marker: the next media packet tells, when it is known.
        int64_t s = hi + 1;
        for (;;) {
            if (lost_.count(s) != 0) {
                // The packet after is lost: it may have been this access
                // unit's last one.
                dead = true;
                return false;
            }
            auto it = packets_.find(s);
            if (it == packets_.end()) {
                return false;
            }
            if (it->second.padding) {
                ++s;
                continue;
            }
            return it->second.timestamp != ts;
        }
    }

    void try_complete_around(int64_t sequence, TimePoint now) {
        int64_t lo = 0;
        int64_t hi = 0;
        if (!group_of(sequence, lo, hi)) {
            return;
        }
        const uint32_t ts = packets_.at(lo).timestamp;
        bool start_dead = false;
        bool end_dead = false;
        const bool start = start_known(lo, ts, start_dead);
        const bool end = end_known(hi, ts, end_dead);
        if (start_dead || end_dead) {
            drop_group(lo, hi);
            return;
        }
        if (!start || !end) {
            return;
        }

        ReconstructedAccessUnit reconstructed;
        if (!reconstruct(lo, hi, reconstructed)) {
            // A corrupt access unit can never decode: as good as lost.
            ++stats_.invalid_h264_packets;
            drop_group(lo, hi);
            report_loss(hi);
            return;
        }

        AccessUnit unit;
        unit.rtp_timestamp = ts;
        unit.first_sequence = lo;
        unit.last_sequence = hi;
        unit.has_idr = reconstructed.has_idr;
        unit.keyframe = reconstructed.has_idr && reconstructed.has_sps && reconstructed.has_pps;
        unit.reference = unit.keyframe ? -1 : reference_of(lo);
        unit.first_arrival = packets_.at(lo).arrival;
        unit.last_arrival = unit.first_arrival;
        for (int64_t s = lo; s <= hi; ++s) {
            Packet& packet = packets_.at(s);
            unit.last_arrival = std::max(unit.last_arrival, packet.arrival);
            unit.retransmitted = unit.retransmitted || packet.repaired;
            release_payload(s, packet);
        }
        unit.annex_b = std::move(reconstructed.bytes);

        ++stats_.complete_access_units;
        if (older_incomplete_exists(lo)) {
            ++stats_.out_of_order_access_units;
        }
        if (unit.keyframe) {
            // Everything before a key frame is obsolete: stop repairing it.
            give_up_before(lo, now, false);
        }
        ++stats_.emitted_access_units;
        if (callbacks_.on_access_unit) {
            callbacks_.on_access_unit(std::move(unit));
        }
    }

    bool older_incomplete_exists(int64_t sequence) const {
        return (!unemitted_.empty() && *unemitted_.begin() < sequence) ||
               (!missing_.empty() && missing_.begin()->first < sequence);
    }

    void drop_group(int64_t lo, int64_t hi) {
        for (int64_t s = lo; s <= hi; ++s) {
            auto it = packets_.find(s);
            if (it != packets_.end()) release_payload(s, it->second);
        }
        ++stats_.incomplete_access_units;
    }

    // Everything before `sequence` is obsolete (a key frame) or gone (a
    // discontinuity): no more repair, no more assembly.
    void give_up_before(int64_t sequence, TimePoint now, bool is_loss) {
        (void)now;
        bool dropped = false;
        while (!unemitted_.empty() && *unemitted_.begin() < sequence) {
            const int64_t s = *unemitted_.begin();
            release_payload(s, packets_.at(s));
            dropped = true;
        }
        if (dropped) ++stats_.incomplete_access_units;
        missing_.erase(missing_.begin(), missing_.lower_bound(sequence));
        if (!have_floor_ || sequence - 1 > floor_) {
            floor_ = sequence - 1;
            have_floor_ = true;
        }
        if (is_loss) {
            report_loss(sequence - 1);
        }
    }

    void report_loss(int64_t sequence) {
        ++stats_.loss_events;
        if (callbacks_.on_loss) {
            callbacks_.on_loss(sequence);
        }
    }

    std::chrono::microseconds nack_repeat() const {
        return std::max<std::chrono::microseconds>(rtt_, kMinNackRepeat);
    }

    // The repair deadline of a missing packet: the render time of the first
    // frame after the gap plus one RTT, and never before the last packet of
    // that frame plus one RTT (a large key frame still arriving), nor before
    // one RTT after the gap was seen.
    TimePoint compute_deadline(int64_t sequence, const Missing& missing) const {
        TimePoint deadline = missing.first_detected + rtt_ + kRepairMargin;
        auto after = packets_.upper_bound(sequence);
        while (after != packets_.end() && after->second.padding) ++after;
        if (after == packets_.end()) {
            return std::max(deadline, missing.first_detected + kDefaultRepairWindow);
        }
        const uint32_t ts = after->second.timestamp;
        std::optional<TimePoint> render;
        if (callbacks_.render_time) render = callbacks_.render_time(ts);
        if (render) {
            deadline = std::max(deadline, *render + rtt_);
        } else {
            deadline = std::max(deadline, missing.first_detected + kDefaultRepairWindow);
        }
        // Progress of that frame.
        TimePoint last = after->second.arrival;
        for (auto it = std::next(after);
             it != packets_.end() && !it->second.padding && it->second.timestamp == ts;
             ++it) {
            last = std::max(last, it->second.arrival);
        }
        return std::max(deadline, last + rtt_ + kRepairMargin);
    }

    void process_nacks(TimePoint now) {
        std::vector<uint16_t> due;
        std::vector<int64_t> expired;
        for (auto& [sequence, missing] : missing_) {
            if (!missing.deadline_known || now >= missing.deadline) {
                // Compute it again when it seems due: the render time and the
                // progress of the frame after the gap can have moved it.
                missing.deadline = compute_deadline(sequence, missing);
                missing.deadline_known = true;
            }
            if (now >= missing.deadline) {
                expired.push_back(sequence);
                continue;
            }

            bool should_send = false;
            if (missing.attempts == 0) {
                should_send =
                    now - missing.first_detected >= kReorderGrace ||
                    missing.newer_packets >= kNewerPacketGrace;
            } else {
                should_send = now - missing.last_nack >= nack_repeat();
            }

            if (should_send) {
                due.push_back(static_cast<uint16_t>(sequence & 0xffff));
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

        if (!expired.empty()) {
            for (const int64_t sequence : expired) {
                missing_.erase(sequence);
                lost_.insert(sequence);
                ++stats_.lost_packets;
            }
            // The access units next to a lost packet can never complete.
            for (const int64_t sequence : expired) {
                drop_dead_around(sequence - 1);
                drop_dead_around(sequence + 1);
            }
            report_loss(expired.back());
        }
    }

    void drop_dead_around(int64_t sequence) {
        int64_t lo = 0;
        int64_t hi = 0;
        if (!group_of(sequence, lo, hi)) return;
        const uint32_t ts = packets_.at(lo).timestamp;
        bool start_dead = false;
        bool end_dead = false;
        start_known(lo, ts, start_dead);
        end_known(hi, ts, end_dead);
        if (start_dead || end_dead) {
            drop_group(lo, hi);
        } else {
            // The loss can settle its start (a NAL that opens an access
            // unit right after the lost packet).
            try_complete_around(lo, last_time_);
        }
    }

    // Packet memory: when the stream is silent, forget what cannot complete.
    void forget_silent(TimePoint now) {
        if (buffered_packets_ == 0 || now - last_any_arrival_ < kPacketMemory) {
            return;
        }
        const bool dropped = !unemitted_.empty();
        while (!unemitted_.empty()) {
            const int64_t s = *unemitted_.begin();
            release_payload(s, packets_.at(s));
        }
        if (dropped) ++stats_.incomplete_access_units;
    }

    // Over the memory bounds: give up the oldest incomplete access unit. A
    // reference is gone with it.
    void enforce_memory_bounds(TimePoint now) {
        (void)now;
        while (buffered_packets_ > kMaxPackets || buffered_bytes_ > kMaxBufferedBytes) {
            if (unemitted_.empty()) break;
            const int64_t oldest = *unemitted_.begin();
            int64_t lo = 0;
            int64_t hi = 0;
            if (!group_of(oldest, lo, hi)) break;
            drop_group(lo, hi);
            ++stats_.buffer_evictions;
            for (int64_t s = lo; s <= hi; ++s) lost_.insert(s);
            report_loss(hi);
        }
    }

    bool append_nal(ReconstructedAccessUnit& output,
                    const uint8_t* nal,
                    size_t nal_size) const {
        constexpr uint8_t start_code[] = {0, 0, 0, 1};
        if (nal_size == 0 || nal_size > kMaxAnnexBBytes - sizeof(start_code) ||
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

    bool reconstruct(int64_t start,
                     int64_t end,
                     ReconstructedAccessUnit& output) const {
        bool fu_open = false;
        uint8_t fu_nal_header = 0;
        std::vector<uint8_t> fu_nal;

        size_t total = 0;
        for (int64_t sequence = start; sequence <= end; ++sequence) {
            total += packets_.at(sequence).payload.size() + 4;
        }
        output.bytes.reserve(total);

        for (int64_t sequence = start; sequence <= end; ++sequence) {
            const Packet& packet = packets_.at(sequence);
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

            if (fu_nal.size() > kMaxAnnexBBytes - (size - 2)) {
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

    Callbacks callbacks_;
    uint8_t payload_type_ = 96;
    std::chrono::microseconds rtt_{30'000};
    Stats stats_;

    // Every packet of the history window by unwrapped sequence number.
    std::map<int64_t, Packet> packets_;
    // The media packets of access units not yet emitted (they hold payload).
    std::set<int64_t> unemitted_;
    std::map<int64_t, Missing> missing_;
    std::set<int64_t> lost_;
    size_t buffered_packets_ = 0;
    size_t buffered_bytes_ = 0;

    bool have_highest_sequence_ = false;
    int64_t highest_sequence_ = 0;
    int64_t base_sequence_ = 0;
    int64_t first_sequence_ = 0;
    // Sequence numbers at or before the floor are obsolete or gone.
    bool have_floor_ = false;
    int64_t floor_ = 0;

    bool have_last_time_ = false;
    TimePoint last_time_{};
    TimePoint last_any_arrival_{};
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

void RtpH264Receiver::set_rtt(std::chrono::microseconds rtt) {
    impl_->set_rtt(rtt);
}

bool RtpH264Receiver::keyframe_in_progress(TimePoint now) const {
    return impl_->keyframe_in_progress(now);
}

RtpH264Receiver::Stats RtpH264Receiver::stats() const {
    return impl_->stats();
}

uint64_t RtpH264Receiver::accepted_packets() const {
    return impl_->accepted_packets();
}

} // namespace mello::transport
