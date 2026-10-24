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
#include <optional>
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

// ---- streaming-quality stage 7: game audio ----

/// Statistics of one stream's game audio (GameAudioSource). Lifetime fields
/// keep the counts of NetEQ instances that a flush or a new stream replaced.
struct GameAudioStats {
    uint64_t packets_inserted = 0;
    /// Not RTP, or not the Opus payload type.
    uint64_t packets_rejected = 0;
    uint64_t insert_errors = 0;
    /// A new SSRC or a timestamp far behind (the host restarted its audio).
    uint64_t stream_resets = 0;
    /// flush() calls that dropped audio (a pause).
    uint64_t flushes = 0;
    // NetEqLifetimeStatistics, summed. Samples per channel at 48 kHz.
    uint64_t total_samples_received = 0;
    uint64_t concealed_samples = 0;
    uint64_t silent_concealed_samples = 0;
    uint64_t concealment_events = 0;
    uint64_t inserted_samples_for_deceleration = 0;
    uint64_t removed_samples_for_acceleration = 0;
    uint64_t fec_packets_received = 0;
    uint64_t fec_packets_discarded = 0;
    uint64_t packets_discarded = 0;
    uint64_t interruption_count = 0;
    uint64_t total_interruption_duration_ms = 0;
    // Now. Zero while no stream plays.
    bool playing = false;
    float target_delay_ms = 0.0f;
    float filtered_delay_ms = 0.0f;
    float buffer_ms = 0.0f;
    /// The minimum delay set through set_minimum_delay_ms (A/V sync).
    int minimum_delay_ms = 0;
};

/// The game audio of one watched stream: one stereo NetEQ, a source of the
/// voice mixer with its own gain (streaming-quality stage 7, spec 12
/// section 9). NetEQ gives it reorder, loss concealment, in-band FEC, drift
/// and a target delay. It plays on the voice output device, after the
/// voice output gain and before the mixer's limiter, and the voice deafen
/// silences it.
///
/// Made by VoicePlayout::add_game_audio(). Destroying it takes it out of
/// the mix (it waits for a mix that runs now). It may outlive the
/// VoicePlayout that made it.
///
/// Threads: insert() any thread (in production the network thread);
/// flush(), set_gain(), set_minimum_delay_ms(), stats() and
/// last_playout() any thread. The device thread pulls it through the mix.
class GameAudioSource {
public:
    ~GameAudioSource();
    GameAudioSource(const GameAudioSource&) = delete;
    GameAudioSource& operator=(const GameAudioSource&) = delete;

    /// Insert one Opus packet. Creates the NetEQ at the first packet after
    /// construction or after flush(). A new SSRC, or a timestamp more than
    /// 2 s behind the newest one, starts a new NetEQ. Returns false when the
    /// packet was rejected or NetEQ refused it.
    bool insert(const RtpInfo& rtp, const uint8_t* payload, size_t payload_size,
                int64_t receive_time_ms);

    /// Drop the audio NetEQ holds: the source plays nothing until the next
    /// packet, which starts a new NetEQ. For a paused stream.
    void flush();

    /// Per-stream gain, clamped to 0..VoicePlayout::kMaxPeerVolume.
    void set_gain(float gain);
    float gain() const;

    /// NetEQ's minimum delay (A/V sync: the video is late). Clamped to
    /// 0..10000 ms. Kept across flush() and new streams.
    void set_minimum_delay_ms(int delay_ms);

    /// The last 10 ms that the mix pulled from this source with audio in it:
    /// the RTP timestamp of its last sample (NetEq::GetPlayoutTimestamp),
    /// the steady time of the pull in microseconds (the test clock when one
    /// is set) and the samples per channel it had.
    struct Playout {
        uint32_t rtp_timestamp = 0;
        int64_t pulled_at_us = 0;
        int samples = 0;
    };
    std::optional<Playout> last_playout() const;

    GameAudioStats stats() const;

private:
    friend class VoicePlayout;
    struct Impl;
    explicit GameAudioSource(std::unique_ptr<Impl> impl);
    std::unique_ptr<Impl> impl_;
};

// ---- end stage 7 ----

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
        /// The same for the game audio of streams (stage 7). Empty: the
        /// upstream defaults.
        std::string game_delay_manager_config;
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
    /// The game audio's tuning that ships (stage 7).
    static const char* default_game_delay_manager_config();

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

    // ---- streaming-quality stage 7 ----
    /// A new game-audio source in this mix (one per watched stream). The
    /// voice statistics (stats(), peer_count(), late_frames()) do not count
    /// it. Any thread.
    std::unique_ptr<GameAudioSource> add_game_audio();
    // ---- end stage 7 ----

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

} // namespace mello::audio
