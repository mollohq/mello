#pragma once

#include <cstddef>
#include <cstdint>
#include <deque>
#include <unordered_map>
#include <vector>

namespace mello::transport {

// RFC 5109 ULPFEC (XOR parity) for the H.264 RTP stream path. One parity
// packet per group of K consecutive media packets recovers exactly one loss
// per group — instant repair with no RTT, complementing NACK retransmission
// (which stays as the multi-loss / fallback path).

inline constexpr uint8_t kUlpfecPayloadType = 127;
inline constexpr char kUlpfecFormatName[] = "ulpfec";
inline constexpr size_t kDefaultFecGroupSize = 10;
// 16-bit mask => max 16 protected packets per group (level 0).
inline constexpr size_t kMaxFecGroupSize = 16;

class UlpfecGenerator {
public:
    explicit UlpfecGenerator(size_t group_size = kDefaultFecGroupSize);

    // Feed one outgoing media RTP packet (marshaled bytes, pre-TWCC-stamp:
    // protected contents must not include the header extension added later
    // in the pacer). When the group completes, pending() returns 0 and the
    // group is retained for exactly one build_packet() call, which the
    // caller must issue before adding more packets.
    void add_packet(const uint8_t* data, size_t size);

    // Marshaled FEC RTP packet: 12-byte RTP header (PT=127, SSRC =
    // media_ssrc+1, own sequence counter), 12-byte ULPFEC header, then the
    // XOR recovery payload. Empty when no completed group is pending (or
    // the group was non-contiguous and got discarded).
    std::vector<uint8_t> build_packet(uint32_t media_ssrc, uint32_t rtp_timestamp);

    // Packets accumulated in the current (incomplete) group. 0 also means
    // a completed group awaits build_packet().
    size_t pending() const { return count_; }

private:
    const size_t group_size_;
    size_t count_ = 0;
    bool ready_ = false;
    uint16_t sn_base_ = 0;
    uint16_t last_seq_ = 0;
    uint32_t last_ts_ = 0;
    bool last_marker_ = false;
    uint16_t mask_ = 0;
    uint32_t ts_recovery_ = 0;
    uint16_t length_recovery_ = 0;
    std::vector<uint8_t> parity_;
    uint16_t fec_sequence_ = 0;
    bool contiguous_ = true;
};

class UlpfecRecovery {
public:
    // Feed every received media packet and every received FEC packet
    // (marshaled bytes). Buffers are bounded to ~2 groups each. The media
    // SSRC is learned from FEC packets (their SSRC minus one).
    void add_media_packet(const uint8_t* data, size_t size);
    void add_fec_packet(const uint8_t* data, size_t size);

    // Attempt to reconstruct the media packet with sequence `sequence`
    // (exactly one loss in its group). On success fills `out` with the
    // marshaled media RTP packet and caches it as received, so the same
    // sequence is never rebuilt twice.
    bool recover(uint16_t sequence, std::vector<uint8_t>& out);

    // Sequences covered by some buffered FEC block's mask but still absent
    // from the media buffer (eager-repair candidates).
    std::vector<uint16_t> uncovered_mask_sequences() const;

    void stats(uint64_t& recovered, uint64_t& unrecoverable) const {
        recovered = recovered_;
        unrecoverable = unrecoverable_;
    }

private:
    struct FecBlock {
        uint16_t sn_base = 0;
        uint16_t mask = 0;
        uint32_t ts_recovery = 0;
        uint16_t length_recovery = 0;
        uint8_t pt_recovery = 0;
        bool marker = false;
        std::vector<uint8_t> recovery_payload;
    };

    static constexpr size_t kMaxMediaPackets = 2 * kMaxFecGroupSize;
    static constexpr size_t kMaxFecBlocks = 4;

    void remember_media(uint16_t sequence, std::vector<uint8_t> packet);

    std::unordered_map<uint16_t, std::vector<uint8_t>> media_;
    std::deque<uint16_t> media_order_;
    std::deque<FecBlock> fec_;
    uint32_t media_ssrc_ = 0;

    uint64_t recovered_ = 0;
    uint64_t unrecoverable_ = 0;
};

} // namespace mello::transport
