#pragma once
// Voice playout engine: one WebRTC NetEQ per remote peer and one
// AudioMixerImpl over them (spec 10 section 5). Each NetEQ runs its own
// DecisionLogic inside a mello controller that handles the sender's
// talk-spurt pre-roll and does not stretch speech after a target jump
// (voice_playout.cpp, TalkSpurtController).
//
// This header has no WebRTC include. The implementation compiles in its own
// C++20 object library (CMake target mello_playout), because the vendored
// NetEQ headers need C++20 and WIN32_LEAN_AND_MEAN, and the rest of libmello
// is C++17.
//
// Threads:
// - insert(), remove_peer(), clear(), set_peer_volume(), set_peer_pan():
//   any thread. In production the network thread inserts.
// - mix(): one thread at a time, the audio device thread (or the caller of
//   the test pull).
// - stats(): any thread.
//
// Locks: one mutex guards the peer map. The device thread never takes it.
// Order: the peer map mutex, then the mixer lock, then a NetEQ lock. mix()
// takes the mixer lock, then each NetEQ lock (NetEqImpl locks itself).

#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>

namespace mello::audio {

/// RTP fields that NetEQ needs. Parsed from a packet by parse_rtp(), or
/// built from the loopback voice frame (spec 10 section 4.2).
struct RtpInfo {
    uint8_t payload_type = 0;
    uint16_t sequence = 0;
    uint32_t timestamp = 0;
    uint32_t ssrc = 0;
};

/// Parse an RTP packet (RFC 3550): fixed header, CSRC list, header
/// extension, padding. On success writes the fields, the payload offset and
/// the payload size, and returns true. Returns false for a packet that is
/// not RTP version 2, is too short, or has no payload.
bool parse_rtp(const uint8_t* packet, size_t size, RtpInfo* info,
               size_t* payload_offset, size_t* payload_size);

/// Receive statistics over every peer. Lifetime fields are totals since the
/// engine was created: they keep the counts of NetEQ instances that were
/// destroyed (peer left, stream reset). "Now" fields are means over the
/// current peers, 0 with no peer.
struct PlayoutStats {
    // NetEqLifetimeStatistics, summed.
    uint64_t total_samples_received = 0;
    uint64_t concealed_samples = 0;
    uint64_t silent_concealed_samples = 0;
    uint64_t concealment_events = 0;
    uint64_t inserted_samples_for_deceleration = 0;
    uint64_t removed_samples_for_acceleration = 0;
    uint64_t fec_packets_received = 0;
    uint64_t fec_packets_discarded = 0;
    uint64_t packets_discarded = 0;
    uint64_t jitter_buffer_delay_ms = 0;
    uint64_t jitter_buffer_emitted_count = 0;
    uint64_t interruption_count = 0;
    uint64_t total_interruption_duration_ms = 0;
    // NetEqOperationsAndState, summed, without the flush that every NetEQ
    // does at its first packet.
    uint64_t packet_buffer_flushes = 0;
    // Engine counters.
    uint64_t packets_inserted = 0;
    uint64_t insert_errors = 0;
    uint64_t get_audio_errors = 0;
    /// Packets dropped before NetEQ: not RTP, or not the Opus payload type.
    uint64_t packets_rejected = 0;
    /// A stream started over on a new SSRC or a far-behind timestamp (a
    /// sender restart). The peer's NetEQ was rebuilt.
    uint64_t stream_resets = 0;
    /// 10 ms frames in which a peer played voice concealment (expand) while
    /// packets for it waited in its NetEQ buffer: late audio. A talk pause
    /// leaves the buffer empty and does not count.
    uint64_t late_frames = 0;
    /// Talk spurts the talk-spurt controller detected (a sender gate gap).
    uint64_t talk_spurts = 0;
    // Now.
    int peers = 0;
    float target_delay_ms = 0.0f;
    float filtered_delay_ms = 0.0f;
    float buffer_ms = 0.0f;
    /// The minimum delay the talk-spurt controller holds for the current
    /// spurt: its pre-roll lateness, 0 below 40 ms.
    float spurt_hold_ms = 0.0f;
};

class VoicePlayout {
public:
    /// 10 ms at 48 kHz.
    static constexpr int kFrameSamples = 480;
    /// The mix is stereo, interleaved.
    static constexpr int kChannels = 2;
    static constexpr uint8_t kOpusPayloadType = 111;
    /// Per-peer volume range. Above 1 a loud peer can clip.
    static constexpr float kMaxPeerVolume = 2.0f;

    struct Config {
        /// Value of the field trial WebRTC-Audio-NetEqDelayManagerConfig.
        std::string delay_manager_config;
        /// NetEq::Config::enable_muted_state: a peer silent for a long time
        /// costs no decode and does not mix.
        bool enable_muted_state = true;
        /// NetEq::Config::enable_fast_accelerate: when the buffer is far
        /// above the target, accelerate removes several pitch periods per
        /// 10 ms, not one. A spurt that a jitter burst left late comes back
        /// to the target within the spurt (voice gate `burst`).
        bool enable_fast_accelerate = true;
        /// The talk-spurt controller's pre-roll handling (spec 10 section
        /// 5.2): the packets of a spurt's first 200 ms stay out of the
        /// arrival statistics, and the spurt plays at its pre-roll lateness.
        /// Off: NetEQ's own arrival statistics and delay. The rule against
        /// preemptive expand after a target jump stays in both cases.
        bool talk_spurt_hold = true;
        /// Spurt lateness below this is network jitter: no delay is held.
        int spurt_hold_floor_ms = 40;
        /// No preemptive expand this long after a target jump.
        int jump_no_expand_ms = 5000;
    };

    /// The delay manager tuning that ships: "forget_factor:0.6,
    /// reorder_forget_factor:0.98", chosen with the voice gate (TESTING.md,
    /// NetEQ tuning).
    static const char* default_delay_manager_config();

    explicit VoicePlayout(Config config);
    ~VoicePlayout();
    VoicePlayout(const VoicePlayout&) = delete;
    VoicePlayout& operator=(const VoicePlayout&) = delete;

    /// Insert one Opus packet for `peer`. Creates the peer's NetEQ on its
    /// first packet. A new SSRC, or a timestamp more than 2 s behind the
    /// last one (a sender restart), rebuilds the peer's NetEQ. Returns false
    /// when the packet was rejected or NetEQ refused it. Any thread.
    bool insert(const std::string& peer, const RtpInfo& rtp, const uint8_t* payload,
                size_t payload_size, int64_t receive_time_ms);
    /// Parse a complete RTP packet with parse_rtp() and insert it. A packet
    /// that does not parse counts as rejected.
    bool insert_rtp(const std::string& peer, const uint8_t* packet, size_t size,
                    int64_t receive_time_ms);

    /// Destroy one peer's NetEQ. Its volume and pan stay set.
    void remove_peer(const std::string& peer);
    /// Destroy every NetEQ. Returns how many there were.
    size_t clear();

    /// Per-peer gain, clamped to 0..kMaxPeerVolume. Stored also for a peer
    /// with no stream yet, and kept across stream resets and clear().
    void set_peer_volume(const std::string& peer, float gain);
    float peer_volume(const std::string& peer) const;
    /// Per-peer pan, clamped to -1 (left) .. +1 (right). 0 = centre (the
    /// default): both channels at unity, so a mono downmix is unchanged.
    void set_peer_pan(const std::string& peer, float pan);
    float peer_pan(const std::string& peer) const;

    /// Output gain (the speaker volume control), applied in each source
    /// before the mixer's limiter. Clamped to 0..kMaxPeerVolume.
    void set_output_gain(float gain);

    /// Mix 10 ms: kFrameSamples stereo frames, interleaved, into `out`
    /// (kFrameSamples * kChannels samples). Calls GetAudio once on every
    /// peer's NetEQ. Returns the number of peers that produced audio
    /// (0 = `out` is silence). Device thread.
    int mix(int16_t* out);

    PlayoutStats stats() const;
    int peer_count() const;
    /// PlayoutStats::late_frames without the peer lock (device thread).
    uint64_t late_frames() const;

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

} // namespace mello::audio
