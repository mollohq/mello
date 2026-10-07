/**
 * @file mello.h
 * @brief Mello C API - Audio, Video, and P2P Transport
 */

#ifndef MELLO_H
#define MELLO_H

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifdef _WIN32
    #ifdef MELLO_EXPORTS
        #define MELLO_API __declspec(dllexport)
    #else
        #define MELLO_API
    #endif
#else
    #define MELLO_API
#endif

/* ============================================================================
 * Types
 * ============================================================================ */

typedef struct MelloContext MelloContext;
typedef struct MelloPeerConnection MelloPeerConnection;

typedef enum MelloResult {
    MELLO_OK = 0,
    MELLO_DEVICE_FALLBACK = 1,
    MELLO_ERROR_INVALID_PARAM = -1,
    MELLO_ERROR_NOT_INITIALIZED = -2,
    MELLO_ERROR_ALREADY_STARTED = -3,
    MELLO_ERROR_FAILED = -4,
    MELLO_ERROR_TRANSPORT_FAILED = -5,
    /** RTP sender queue full or awaiting IDR — producer should drop/coalesce. */
    MELLO_ERROR_TRANSPORT_BACKPRESSURE = -6,
} MelloResult;

typedef struct MelloIceCandidate {
    const char* candidate;
    const char* sdp_mid;
    int sdp_mline_index;
} MelloIceCandidate;

/** Log callback: level (0=debug,1=info,2=warn,3=error), tag, message. */
typedef void (*MelloLogCallback)(void* user_data, int level, const char* tag, const char* message);

typedef enum MelloMicPermission {
    MELLO_MIC_NOT_DETERMINED = 0,
    MELLO_MIC_GRANTED = 1,
    MELLO_MIC_DENIED = 2,
} MelloMicPermission;

typedef enum MelloNsMode {
    MELLO_NS_OFF              = 0,
    MELLO_NS_RNNOISE          = 1,
    MELLO_NS_WEBRTC_LOW       = 2,
    MELLO_NS_WEBRTC_MODERATE  = 3,
    MELLO_NS_WEBRTC_HIGH      = 4,
    MELLO_NS_WEBRTC_VERY_HIGH = 5,
} MelloNsMode;

typedef void (*MelloVoiceActivityCallback)(void* user_data, bool speaking);
typedef void (*MelloMicPermissionCallback)(void* user_data, bool granted);
typedef void (*MelloIceCandidateCallback)(void* user_data, const MelloIceCandidate* candidate);
typedef void (*MelloPeerStateCallback)(void* user_data, int state);
typedef void (*MelloPeerDataCallback)(void* user_data, const uint8_t* data, int size, bool reliable);
typedef void (*MelloAudioTrackCallback)(void* user_data, const char* sender_id, const uint8_t* data, int size);
/** One text message from the reliable control DataChannel. `text` is not
 *  NUL-terminated; `size` is its length in bytes. */
typedef void (*MelloControlTextCallback)(void* user_data, const char* text, int size);

/** What the remote end (the SFU) reports about the audio this peer sends:
 *  one RTCP receiver report block about the send track's SSRC (RFC 3550
 *  section 6.4.1). Spec 10 section 7.5. */
typedef struct MelloSendFeedback {
    /** Report blocks about the send track since the peer was created. */
    uint32_t reports;
    /** Fraction of packets lost since the previous report, 0..1. */
    float    fraction_lost;
    /** Cumulative packets lost (signed 24-bit on the wire). */
    int32_t  cumulative_lost;
    /** Extended highest sequence number received. */
    uint32_t extended_highest_seq;
    /** Interarrival jitter in ms. */
    float    jitter_ms;
    /** Round trip from the report's LSR and DLSR against this peer's sender
     *  reports, in ms. -1 when unknown (no sender report reached the remote). */
    float    rtt_ms;
    /** Packets expected since the previous report: the difference of the
     *  extended highest sequence numbers. 0 when unknown. */
    uint32_t interval_expected;
    /** Packets lost since the previous report: the difference of the
     *  cumulative counts, or fraction_lost x interval_expected when that
     *  difference is not usable. At most interval_expected. */
    uint32_t interval_lost;
} MelloSendFeedback;

/** Receives one MelloSendFeedback per receiver report about the send track.
 *  Called on the libdatachannel thread; must return fast. */
typedef void (*MelloSendFeedbackCallback)(void* user_data, const MelloSendFeedback* feedback);

/* ============================================================================
 * Context
 * ============================================================================ */

MELLO_API MelloContext* mello_init(void);
MELLO_API void mello_destroy(MelloContext* ctx);
MELLO_API const char* mello_get_error(MelloContext* ctx);

/** Set a log callback to receive all libmello log output. Pass NULL to revert to stderr. */
MELLO_API void mello_set_log_callback(MelloLogCallback callback, void* user_data);

/** Set the libmello log verbosity at runtime (0=debug,1=info,2=warn,3=error).
 *  Used by diagnostic capture to temporarily raise verbosity. Out-of-range values are ignored. */
MELLO_API void mello_set_log_level(int level);

/* ============================================================================
 * Microphone Permission (macOS: AVCaptureDevice; others: always granted)
 * ============================================================================ */

MELLO_API MelloMicPermission mello_mic_permission_status(void);
MELLO_API void mello_mic_request_permission(MelloMicPermissionCallback callback, void* user_data);

/* ============================================================================
 * Voice
 * ============================================================================ */

MELLO_API MelloResult mello_voice_start_capture(MelloContext* ctx);
MELLO_API MelloResult mello_voice_stop_capture(MelloContext* ctx);
MELLO_API void mello_voice_set_mute(MelloContext* ctx, bool muted);
MELLO_API void mello_voice_set_deafen(MelloContext* ctx, bool deafened);
/** When enabled, Silero VAD and the adaptive RMS speech gate are bypassed while unmuted. */
MELLO_API void mello_voice_set_push_to_talk(MelloContext* ctx, bool enabled);
MELLO_API bool mello_voice_is_speaking(MelloContext* ctx);

MELLO_API void mello_voice_set_vad_callback(
    MelloContext* ctx,
    MelloVoiceActivityCallback callback,
    void* user_data
);

/** Enable/disable echo cancellation (AEC3). Enabled by default. */
MELLO_API void mello_voice_set_echo_cancellation(MelloContext* ctx, bool enabled);

/** Enable/disable neural residual-echo suppression (post-AEC mask model).
 *  Disabled by default. Soft dependency: without the model file the stage
 *  is a passthrough and the toggle has no audible effect. */
MELLO_API void mello_voice_set_echo_suppression(MelloContext* ctx, bool enabled);

/** Enable/disable automatic gain control (AGC2). Enabled by default. */
MELLO_API void mello_voice_set_agc(MelloContext* ctx, bool enabled);

/** Enable/disable RNNoise suppression. Enabled by default. */
MELLO_API void mello_voice_set_noise_suppression(MelloContext* ctx, bool enabled);
MELLO_API void mello_voice_set_ns_mode(MelloContext* ctx, MelloNsMode mode);
MELLO_API void mello_voice_set_transient_suppression(MelloContext* ctx, bool enabled);
MELLO_API void mello_voice_set_high_pass_filter(MelloContext* ctx, bool enabled);

/** Set input (microphone) volume. 0.0 = silent, 1.0 = unity gain. */
MELLO_API void mello_voice_set_input_volume(MelloContext* ctx, float volume);

/** Set output (speaker) volume. 0.0 = silent, 1.0 = unity gain. */
MELLO_API void mello_voice_set_output_volume(MelloContext* ctx, float volume);

/** Get current input audio level (0.0 = silence, 1.0 = peak). Updated per frame. */
MELLO_API float mello_voice_get_input_level(MelloContext* ctx);

/** Manual input sensitivity in dBFS (-100..0, clamped). With auto off, the
 *  speech gate opens when the raw microphone frame RMS reaches `db` dBFS
 *  (the scale of the input level meter). Silero VAD then confirms speech at
 *  its fixed threshold. Stored also while auto is on. Default -40.
 *  No effect in push-to-talk mode, which bypasses the gate. */
MELLO_API void mello_voice_set_input_sensitivity(MelloContext* ctx, float db);

/** Auto input sensitivity (default on): the speech gate tracks the ambient
 *  noise floor (2.5 x the floor, at least -54 dBFS). Off: the gate uses the
 *  level from mello_voice_set_input_sensitivity. */
MELLO_API void mello_voice_set_input_sensitivity_auto(MelloContext* ctx, bool enabled);

/** Get next encoded audio packet to send to peers. Returns packet size, or 0 if none. */
MELLO_API int mello_voice_get_packet(MelloContext* ctx, uint8_t* buffer, int buffer_size);

/** As mello_voice_get_packet, and writes the packet's media timestamp to
 *  `timestamp` (may be NULL): the 48 kHz capture sample index of the frame's
 *  first sample. It counts every captured 20 ms frame, encoded or not, so a
 *  gap from the speech gate or mute advances it like DTX. Two consecutive
 *  frames differ by 960. Pass it to mello_peer_send_audio_frame. The packet
 *  bytes are the same as mello_voice_get_packet returns. */
MELLO_API int mello_voice_get_packet_with_timestamp(
    MelloContext* ctx,
    uint8_t* buffer,
    int buffer_size,
    uint32_t* timestamp
);

/** Receives one encoded voice frame (spec 10 section 4.5).
 *  `data`: the Opus payload, without the 4-byte sequence header of
 *  mello_voice_get_packet. Valid only during the call.
 *  `timestamp`: the media time in 48 kHz samples, as
 *  mello_voice_get_packet_with_timestamp reports it.
 *  `sequence`: the 32-bit packet counter (the 4-byte header value). */
typedef void (*MelloPacketSinkCallback)(
    void* user_data,
    const uint8_t* data,
    int size,
    uint32_t timestamp,
    uint32_t sequence
);

/** Set the voice packet sink, or clear it with callback = NULL.
 *
 *  While a sink is set, libmello calls it for each encoded frame right after
 *  Opus encode, and the frame does not go to the mello_voice_get_packet
 *  queue. Setting a sink drops the frames in that queue. Mute and
 *  push-to-talk gate frames before encode, as for the queue.
 *
 *  Thread: the sink runs on the audio capture thread (or on the thread that
 *  calls mello_voice_inject_capture), with the capture lock held. It must
 *  return fast and must not block. It may call mello_peer_send_audio_frame
 *  and mello_voice_feed_packet. It must not call this function or any
 *  capture control of the same context (start, stop, inject, push-to-talk).
 *
 *  Lifetime: set and clear wait for a sink call that runs now. After a clear
 *  returns, no call uses `user_data`, so the caller may free it. The context
 *  clears the sink in mello_destroy. Any thread, never from inside the sink.
 *  Returns MELLO_ERROR_INVALID_PARAM for a NULL context. */
MELLO_API MelloResult mello_voice_set_packet_sink(
    MelloContext* ctx,
    MelloPacketSinkCallback callback,
    void* user_data
);

/** Encoder adaptation to the send leg (spec 10 section 4.6). Give each
 *  receiver report about the voice send track to the context (from
 *  MelloSendFeedbackCallback). libmello steps the Opus bitrate down a ladder
 *  (the channel bitrate, then 64, 48, 32, 24 kbps) when the loss over the
 *  last 2 s exceeds 10 % or the round trip doubles, and up one rung after
 *  10 s clean. OPUS_SET_PACKET_LOSS_PERC follows the loss of the last 4 s,
 *  5 to 30. The capture thread applies the result before the next encode.
 *  Any thread; it never waits for the capture thread. The time is the
 *  playout clock (mello_test_set_clock_ms in tests). */
MELLO_API void mello_voice_report_send_feedback(MelloContext* ctx, const MelloSendFeedback* feedback);

/** The channel bitrate in kbps: the top rung of the ladder. Clamped to
 *  24..96. Default 64. At the top rung the encoder moves to the new value
 *  at the next frame. Any thread. */
MELLO_API void mello_voice_set_max_bitrate(MelloContext* ctx, int kbps);

/** Feed one voice frame received from a peer on the P2P data channel, or
 *  from the local mic test (loopback). Spec 10 section 4.2. The frame is an
 *  8-byte header, then the Opus payload:
 *    bytes 0..3  the 32-bit sender sequence (mello_voice_get_packet header),
 *                little-endian
 *    bytes 4..7  the RTP timestamp (mello_voice_get_packet_with_timestamp),
 *                little-endian
 *  libmello builds the RTP fields from it (payload type 111, the low 16 bits
 *  of the sequence, the timestamp, SSRC 0) and inserts the frame into the
 *  peer's NetEQ, the same path as mello_voice_feed_rtp. A frame of 8 bytes
 *  or less is ignored. Any thread. */
MELLO_API MelloResult mello_voice_feed_packet(
    MelloContext* ctx,
    const char* peer_id,
    const uint8_t* data,
    int size
);

/** Feed one complete RTP packet (header and Opus payload) received from the
 *  SFU for `peer_id`. Spec 10 section 5.1. NetEQ uses the payload type,
 *  sequence, timestamp and SSRC. A new SSRC for the same peer (the SFU
 *  reassigned it) or a timestamp more than 2 s behind the newest one (the
 *  sender restarted) rebuilds that peer's NetEQ. Payload types other than
 *  111 (Opus) and 13 (CN) are dropped.
 *  `receive_time_ms`: arrival time on the playout clock; < 0 lets libmello
 *  stamp it (the steady clock, or the test clock of mello_test_set_clock_ms).
 *  Any thread; in production the libdatachannel track thread. */
MELLO_API MelloResult mello_voice_feed_rtp(
    MelloContext* ctx,
    const char* peer_id,
    const uint8_t* rtp_packet,
    int size,
    int64_t receive_time_ms
);

/** Destroy one peer's receive state (its NetEQ), for example when the peer
 *  leaves the channel. Its volume and pan settings stay. A later packet from
 *  the peer starts a new stream. mello_voice_stop_capture destroys every
 *  peer's state. Any thread. */
MELLO_API void mello_voice_remove_peer(MelloContext* ctx, const char* peer_id);

/** Per-peer playback volume. 0.0 = silent, 1.0 = unity (the default), up to
 *  2.0 (clamped). Above 1.0 a loud peer can clip. Applied to the peer's
 *  stream before the mixer's limiter, together with the output volume. Kept
 *  for a peer with no stream yet, and across leave and rejoin of the
 *  context's voice session. Any thread. */
MELLO_API void mello_voice_set_peer_volume(MelloContext* ctx, const char* peer_id, float gain);

/** Per-peer stereo pan. -1.0 = left, 0.0 = centre (the default), +1.0 =
 *  right (clamped). Balance law: the far channel fades, the near channel
 *  stays at unity. A mono output device plays the downmix. Any thread. */
MELLO_API void mello_voice_set_peer_pan(MelloContext* ctx, const char* peer_id, float pan);

/** The per-peer volume that libmello applies (1.0 when never set). */
MELLO_API float mello_voice_get_peer_volume(MelloContext* ctx, const char* peer_id);

/** The per-peer pan that libmello applies (0.0 when never set). */
MELLO_API float mello_voice_get_peer_pan(MelloContext* ctx, const char* peer_id);

MELLO_API MelloResult mello_voice_start_capture_inject(MelloContext* ctx);
MELLO_API void mello_voice_inject_capture(MelloContext* ctx, const int16_t* samples, int count);
MELLO_API void mello_voice_stop_capture_inject(MelloContext* ctx);

/* ============================================================================
 * Voice test hooks (voice quality gate, mello-sys/tests/voice_gate.rs)
 *
 * Not for production use. Set the environment variable
 * MELLO_AUDIO_BACKEND=test before mello_init() to select a device-free audio
 * backend: no capture or playback device opens and no device thread starts.
 * Feed capture audio with mello_voice_start_capture_inject() and
 * mello_voice_inject_capture(), and pull mixed output with
 * mello_voice_test_pull_output(). Without the variable, nothing changes.
 * ============================================================================ */

/** Pull `count` samples (48 kHz mono int16) of mixed voice output through the
 *  playback render path, as the playback device thread does. The test
 *  backend is mono, so this is the downmix of the stereo mix (a centred peer
 *  plays the same on both channels). Returns `count` when any of it came
 *  from a peer or a clip, 0 when all of it is silence, or -1 when the
 *  context does not use the test backend. */
MELLO_API int mello_voice_test_pull_output(MelloContext* ctx, int16_t* out, int count);

/** Drive the playout clock from the caller: the receive time that
 *  mello_voice_feed_packet and mello_voice_feed_rtp stamp, and NetEQ's
 *  clock. NetEQ times its playout by its GetAudio calls (10 ms each), so the
 *  caller also paces mello_voice_test_pull_output.
 *  now_ms >= 0 sets the clock to that value in milliseconds. now_ms < 0
 *  restores the steady clock. Process-wide: it affects every context.
 *  With MELLO_AUDIO_BACKEND=test, two more variables apply to contexts
 *  created after they are set, for tuning and measurement runs:
 *  MELLO_NETEQ_DELAY_MANAGER_CONFIG replaces the NetEQ delay manager field
 *  trial value ("default" = upstream defaults), and
 *  MELLO_VOICE_PRE_ROLL_FRAMES sets the sender pre-roll in frames (0..50). */
MELLO_API void mello_test_set_clock_ms(int64_t now_ms);

/* ============================================================================
 * Clip Buffer
 * ============================================================================ */

/** Start the rolling voice clip buffer. Call when joining a voice channel. */
MELLO_API MelloResult mello_clip_buffer_start(MelloContext* ctx);

/** Stop and discard the clip buffer. Call when leaving voice. */
MELLO_API MelloResult mello_clip_buffer_stop(MelloContext* ctx);

/** Returns true if the clip buffer is actively recording. */
MELLO_API bool mello_clip_buffer_active(MelloContext* ctx);

/** Capture the last `seconds` of audio and write as WAV to `output_path`. */
MELLO_API MelloResult mello_clip_capture(MelloContext* ctx, float seconds, const char* output_path);

/** Play a WAV clip through the audio output. */
MELLO_API MelloResult mello_clip_play(MelloContext* ctx, const char* wav_path);

/** Play an MP4/AAC clip through the audio output (decodes to PCM first). */
MELLO_API MelloResult mello_clip_play_mp4(MelloContext* ctx, const char* mp4_path);

/** Stop clip playback. */
MELLO_API MelloResult mello_clip_stop_playback(MelloContext* ctx);

/** Returns true if a clip is currently playing (or paused mid-playback). */
MELLO_API bool mello_clip_is_playing(MelloContext* ctx);

/** Get playback progress. All out-params are optional (may be NULL). */
MELLO_API void mello_clip_playback_progress(MelloContext* ctx,
    uint64_t* position_samples, uint64_t* total_samples, uint32_t* sample_rate);

/** Pause clip playback. */
MELLO_API MelloResult mello_clip_pause(MelloContext* ctx);

/** Resume clip playback after pause. */
MELLO_API MelloResult mello_clip_resume(MelloContext* ctx);

/** Seek clip playback to an absolute sample position. */
MELLO_API MelloResult mello_clip_seek(MelloContext* ctx, uint64_t position_samples);

/** Encode a WAV file to MP4/AAC-LC. Standalone (no MelloContext needed). */
MELLO_API MelloResult mello_clip_encode(const char* wav_path, const char* mp4_path, int bitrate);

/* ============================================================================
 * P2P Transport
 * ============================================================================ */

typedef enum MelloPeerMediaRole {
    MELLO_PEER_MEDIA_ROLE_VOICE = 0,
    MELLO_PEER_MEDIA_ROLE_STREAM_HOST = 1,
    MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER = 2,
} MelloPeerMediaRole;

typedef enum MelloPeerVideoFeedbackType {
    MELLO_PEER_VIDEO_FEEDBACK_PLI = 0,
    MELLO_PEER_VIDEO_FEEDBACK_REMB = 1,
    MELLO_PEER_VIDEO_FEEDBACK_LOCAL_IDR_NEEDED = 2,
    MELLO_PEER_VIDEO_FEEDBACK_GCC_TARGET = 3,
} MelloPeerVideoFeedbackType;

typedef struct MelloPeerVideoFeedback {
    MelloPeerVideoFeedbackType type;
    uint32_t remb_bitrate_bps;
} MelloPeerVideoFeedback;

typedef struct MelloRtpVideoAccessUnitInfo {
    uint32_t size;
    uint8_t is_idr;
    uint32_t rtp_timestamp;
    uint64_t capture_timestamp_us;
} MelloRtpVideoAccessUnitInfo;

/** Unambiguous error result from mello_peer_video_recv_access_unit(). */
#define MELLO_PEER_VIDEO_RECV_ERROR INT32_MIN

typedef struct MelloRtpVideoStats {
    uint8_t media_role;
    uint8_t video_open;
    uint8_t tx_active;
    uint8_t rx_active;

    uint64_t tx_access_units_enqueued;
    uint64_t tx_access_units_sent;
    uint64_t tx_access_units_dropped;
    uint64_t tx_access_units_rejected;
    uint64_t tx_bytes_sent;
    uint64_t tx_send_failures;
    uint64_t tx_rtp_packets_sent;
    uint64_t tx_rtp_wire_bytes_sent;
    uint64_t tx_queued_access_units;
    uint64_t tx_peak_queued_access_units;
    uint64_t tx_queued_bytes;
    uint64_t tx_peak_queued_bytes;
    uint64_t tx_pacing_target_bps;
    uint64_t tx_current_pacing_delay_us;
    uint64_t tx_max_pacing_delay_us;
    uint64_t tx_local_idr_requests;
    uint64_t tx_pli_requests;
    uint64_t tx_remb_reports;
    uint32_t tx_latest_remb_bitrate_bps;
    uint64_t tx_rtx_requests;
    uint64_t tx_rtx_sent;
    uint64_t tx_rtx_cache_misses;
    uint64_t tx_rtx_queue_dropped;
    uint64_t tx_twcc_reports;
    uint64_t tx_gcc_target_bps;
    uint64_t tx_fec_packets_sent;

    uint64_t rx_ingress_packets;
    uint64_t rx_ingress_bytes;
    uint64_t rx_ingress_dropped_packets;
    uint64_t rx_ingress_dropped_bytes;
    uint64_t rx_ingress_overflows;
    uint64_t rx_ingress_queued_packets;
    uint64_t rx_ingress_queued_bytes;
    uint64_t rx_peak_ingress_queued_packets;
    uint64_t rx_peak_ingress_queued_bytes;
    uint64_t rx_wrong_ssrc_packets_after_recovery;
    uint64_t rx_access_units_queued_total;
    uint64_t rx_access_unit_bytes_queued_total;
    uint64_t rx_access_units_dropped;
    uint64_t rx_access_unit_bytes_dropped;
    uint64_t rx_output_queued_access_units;
    uint64_t rx_output_queued_bytes;
    uint64_t rx_peak_output_queued_access_units;
    uint64_t rx_peak_output_queued_bytes;
    uint64_t rx_nack_packets_sent;
    uint64_t rx_nack_sequences_sent;
    uint64_t rx_pli_requests;
    uint64_t rx_pli_packets_sent;
    uint64_t rx_remb_packets_sent;
    uint64_t rx_twcc_packets_sent;
    uint64_t rx_fec_recovered;
    uint64_t rx_fec_unrecoverable;
    uint64_t rx_receiver_reports_sent;
    uint64_t rx_sender_reports_received;
    uint64_t rx_invalid_rtcp_packets;
    uint64_t rx_feedback_send_failures;
    uint64_t rx_core_restarts;
    uint32_t rx_payload_type;
    uint32_t rx_local_feedback_ssrc;
    uint32_t rx_remote_media_ssrc;
    uint32_t rx_receive_target_bps;
    uint8_t rx_has_remote_media_ssrc;
    uint8_t rx_awaiting_output_idr;

    uint64_t rx_core_packets;
    uint64_t rx_core_bytes_received;
    uint64_t rx_core_accepted_packets;
    uint64_t rx_core_accepted_bytes;
    uint64_t rx_core_duplicates;
    uint64_t rx_core_late_packets;
    uint64_t rx_core_invalid_rtp_packets;
    uint64_t rx_core_invalid_h264_packets;
    uint64_t rx_core_wrong_payload_type_packets;
    uint64_t rx_core_wrong_ssrc_packets;
    uint64_t rx_core_backwards_time_inputs;
    uint64_t rx_core_missing_sequences_detected;
    uint64_t rx_core_repaired_packets;
    uint64_t rx_core_nacks;
    uint64_t rx_core_nack_callbacks;
    uint64_t rx_core_complete_access_units;
    uint64_t rx_core_incomplete_access_units;
    uint64_t rx_core_emitted_access_units;
    uint64_t rx_core_pli_requests;
    uint64_t rx_core_gate_dropped_access_units;
    uint64_t rx_core_gate_entries;
    uint64_t rx_core_gate_exits;
    uint64_t rx_core_buffer_evictions;
    uint64_t rx_core_sequence_discontinuities;
    uint64_t rx_core_buffered_access_units;
    uint64_t rx_core_buffered_packets;
    uint64_t rx_core_buffered_bytes;
    uint64_t rx_core_peak_buffered_access_units;
    uint64_t rx_core_peak_buffered_packets;
    uint64_t rx_core_peak_buffered_bytes;
    uint8_t rx_core_has_ssrc;
    uint32_t rx_core_ssrc;
    uint64_t rx_core_extended_highest_sequence;
    uint64_t rx_core_cumulative_loss;
    uint32_t rx_core_interarrival_jitter;
    uint8_t rx_core_gated;
} MelloRtpVideoStats;

MELLO_API MelloPeerConnection* mello_peer_create(MelloContext* ctx, const char* peer_id);
MELLO_API MelloPeerConnection* mello_peer_create_for_role(
    MelloContext* ctx,
    const char* peer_id,
    MelloPeerMediaRole role
);
MELLO_API void mello_peer_destroy(MelloPeerConnection* peer);

MELLO_API void mello_peer_set_ice_servers(
    MelloPeerConnection* peer,
    const char** urls,
    int count
);

MELLO_API const char* mello_peer_create_offer(MelloPeerConnection* peer);
MELLO_API const char* mello_peer_create_answer(MelloPeerConnection* peer, const char* offer_sdp);

MELLO_API MelloResult mello_peer_set_remote_description(
    MelloPeerConnection* peer,
    const char* sdp,
    bool is_offer
);

MELLO_API MelloResult mello_peer_add_ice_candidate(
    MelloPeerConnection* peer,
    const MelloIceCandidate* candidate
);

MELLO_API void mello_peer_set_ice_callback(
    MelloPeerConnection* peer,
    MelloIceCandidateCallback callback,
    void* user_data
);

MELLO_API void mello_peer_set_state_callback(
    MelloPeerConnection* peer,
    MelloPeerStateCallback callback,
    void* user_data
);

MELLO_API void mello_peer_set_data_callback(
    MelloPeerConnection* peer,
    MelloPeerDataCallback callback,
    void* user_data
);

/** Incoming audio for stream viewers and tools: `data` is the Opus payload
 *  behind a 4-byte header [seq_lo, seq_hi, 0, 0] (the 16-bit RTP sequence,
 *  little-endian). The RTP timestamp is not carried. Voice uses
 *  mello_peer_set_audio_rtp_callback. */
MELLO_API void mello_peer_set_audio_track_callback(
    MelloPeerConnection* peer,
    MelloAudioTrackCallback callback,
    void* user_data
);

/** Incoming audio as complete RTP packets (header and Opus payload), for
 *  mello_voice_feed_rtp. Same callback type as the audio track callback:
 *  `data` and `size` are the whole RTP packet, valid only during the call.
 *  Only payload type 111 (Opus) is delivered. Both callbacks may be set;
 *  each gets every packet. Called on the libdatachannel track thread. */
MELLO_API void mello_peer_set_audio_rtp_callback(
    MelloPeerConnection* peer,
    MelloAudioTrackCallback callback,
    void* user_data
);

MELLO_API MelloResult mello_peer_send_unreliable(
    MelloPeerConnection* peer,
    const uint8_t* data,
    int size
);

MELLO_API MelloResult mello_peer_send_reliable(
    MelloPeerConnection* peer,
    const uint8_t* data,
    int size
);

MELLO_API bool mello_peer_is_connected(MelloPeerConnection* peer);
MELLO_API bool mello_peer_is_unreliable_open(MelloPeerConnection* peer);
MELLO_API bool mello_peer_is_reliable_open(MelloPeerConnection* peer);

/** Send raw Opus frame via the RTP audio track. Packetization is automatic.
 *  The RTP timestamp does not advance with this call: every packet carries
 *  the track's start timestamp. Use mello_peer_send_audio_frame. */
MELLO_API MelloResult mello_peer_send_audio(MelloPeerConnection* peer, const uint8_t* data, int size);

/** Send one Opus frame via the RTP audio track. `timestamp` is the frame's
 *  media time in 48 kHz samples from any origin (for voice:
 *  mello_voice_get_packet_with_timestamp). The RTP timestamp of the packet is
 *  the track's random start timestamp plus `timestamp`, modulo 2^32. */
MELLO_API MelloResult mello_peer_send_audio_frame(
    MelloPeerConnection* peer,
    const uint8_t* data,
    int size,
    uint32_t timestamp
);

/** Handle a server-initiated SDP renegotiation offer. Returns answer SDP. */
MELLO_API const char* mello_peer_handle_remote_offer(MelloPeerConnection* peer, const char* offer_sdp);

/** Poll next received unreliable packet. Returns bytes copied, 0 if empty. */
MELLO_API int mello_peer_recv(MelloPeerConnection* peer, uint8_t* buffer, int buffer_size);

/** Send a ping on the reliable DataChannel for RTT measurement. */
MELLO_API void mello_peer_send_ping(MelloPeerConnection* peer);

/** Get the smoothed RTT in milliseconds (0 if no measurement yet). */
MELLO_API float mello_peer_rtt_ms(MelloPeerConnection* peer);

/** Age in milliseconds since the last control-channel pong (-1 if none seen). */
MELLO_API int64_t mello_peer_pong_age_ms(MelloPeerConnection* peer);

/** Number of send_audio calls that were skipped because the track wasn't open. */
MELLO_API int mello_peer_send_audio_skips(MelloPeerConnection* peer);

/** Number of incoming RTP tracks currently wired (recv_track_count). For a
 *  voice peer with receive slots: the count of mids that carry a sender. */
MELLO_API int mello_peer_recv_track_count(MelloPeerConnection* peer);

/** Receive slots in an SFU voice offer (mello-sfu 01-SFU.md section 5.5). */
#define MELLO_VOICE_RECV_SLOTS 16

/** Voice SFU receive slots: the next mello_peer_create_offer of this voice
 *  peer carries `count` recvonly audio m-lines (mids "s0", "s1", ...) after
 *  its sendrecv mic m-line. The SFU forwards one member on each and names
 *  the member with a `slot` control message (mello_peer_set_audio_mid_sender).
 *  Default 0, for P2P. Clamped to 0..64. Ignored for stream roles. */
MELLO_API void mello_peer_set_voice_recv_slots(MelloPeerConnection* peer, int count);

/** Voice: incoming RTP on m-line `mid` is from `sender_id` from now on. NULL
 *  or "" frees the mid: its packets are dropped. The audio callbacks get
 *  `sender_id` as the sender. Thread safe. Mids reset with each new offer. */
MELLO_API MelloResult mello_peer_set_audio_mid_sender(
    MelloPeerConnection* peer,
    const char* mid,
    const char* sender_id
);

/** Text messages on the reliable control DataChannel, except `pong` (RTT is
 *  handled inside). Called on the libdatachannel thread; must return fast. */
MELLO_API void mello_peer_set_control_text_callback(
    MelloPeerConnection* peer,
    MelloControlTextCallback callback,
    void* user_data
);

/** Send one text message on the reliable control DataChannel. Thread safe.
 *  MELLO_ERROR_TRANSPORT_FAILED when the channel is not open. */
MELLO_API MelloResult mello_peer_send_control_text(MelloPeerConnection* peer, const char* text);

/** Set or clear (NULL) the send feedback callback. Any thread. A call that
 *  runs now can still use the old `user_data`; the caller keeps it valid
 *  until mello_peer_destroy. */
MELLO_API void mello_peer_set_send_feedback_callback(
    MelloPeerConnection* peer,
    MelloSendFeedbackCallback callback,
    void* user_data
);

/** The feedback of the last receiver report about the send track. Returns
 *  false (and leaves `out` unchanged) when no report arrived yet. Any thread. */
MELLO_API bool mello_peer_get_send_feedback(MelloPeerConnection* peer, MelloSendFeedback* out);

/** Send one Annex-B H.264 access unit on a StreamHost peer. */
MELLO_API MelloResult mello_peer_video_send_access_unit(
    MelloPeerConnection* peer,
    const uint8_t* data,
    int size,
    uint64_t capture_ts_us
);

/**
 * Poll one received access unit.
 * 0 means no access unit is queued. A positive result is the byte count copied.
 * A negative result other than MELLO_PEER_VIDEO_RECV_ERROR is the required
 * capacity; that access unit remains queued. MELLO_PEER_VIDEO_RECV_ERROR means
 * invalid input, an impossible size conversion, or an internal exception.
 */
MELLO_API int mello_peer_video_recv_access_unit(
    MelloPeerConnection* peer,
    uint8_t* buffer,
    int capacity,
    MelloRtpVideoAccessUnitInfo* info
);

/** Poll one queued host-side video feedback event. Returns false when empty. */
MELLO_API uint8_t mello_peer_video_take_feedback(
    MelloPeerConnection* peer,
    MelloPeerVideoFeedback* feedback
);

MELLO_API MelloResult mello_peer_video_set_pacing_target(
    MelloPeerConnection* peer,
    uint64_t bps
);

MELLO_API MelloResult mello_peer_video_set_receive_target(
    MelloPeerConnection* peer,
    uint32_t bps
);

MELLO_API void mello_peer_video_get_stats(
    MelloPeerConnection* peer,
    MelloRtpVideoStats* stats
);

MELLO_API uint8_t mello_peer_video_is_open(MelloPeerConnection* peer);

/* ============================================================================
 * Video / Streaming
 * ============================================================================ */

typedef struct MelloStreamHost MelloStreamHost;
typedef struct MelloStreamView MelloStreamView;

typedef enum MelloCodec {
    MELLO_CODEC_H264 = 0,
    MELLO_CODEC_AV1  = 1,
} MelloCodec;

typedef enum MelloEncoderBackend {
    MELLO_ENCODER_NVENC        = 0,
    MELLO_ENCODER_AMF          = 1,
    MELLO_ENCODER_QSV          = 2,
    MELLO_ENCODER_VIDEOTOOLBOX = 3, // macOS (Apple Silicon)
} MelloEncoderBackend;

typedef enum MelloDecoderBackend {
    MELLO_DECODER_NVDEC        = 0,
    MELLO_DECODER_AMF          = 1,
    MELLO_DECODER_D3D11VA      = 2,
    MELLO_DECODER_OPENH264     = 3,
    MELLO_DECODER_DAV1D        = 4, // retired: kept for ABI stability, never reported
    MELLO_DECODER_VIDEOTOOLBOX = 5, // macOS (Apple Silicon)
} MelloDecoderBackend;

/** Returns available encoder backends on this machine, in priority order. */
MELLO_API int mello_get_encoders(MelloContext* ctx, MelloEncoderBackend* out, int max_count);

/** Returns available decoder backends on this machine, in priority order. */
MELLO_API int mello_get_decoders(MelloContext* ctx, MelloDecoderBackend* out, int max_count);

/** Returns true if a HW encoder (NVENC/AMF/QSV) is available on this machine. */
MELLO_API bool mello_encoder_available(MelloContext* ctx);

/* ---- Capture source ---- */

typedef enum MelloCaptureMode {
    MELLO_CAPTURE_MONITOR = 0,
    MELLO_CAPTURE_WINDOW  = 1,
    MELLO_CAPTURE_PROCESS = 2,
    /* Monitor captured with Windows Graphics Capture instead of DXGI desktop
     * duplication. For the DXGI vs WGC benchmark; Windows only. */
    MELLO_CAPTURE_MONITOR_WGC = 3,
} MelloCaptureMode;

typedef struct MelloCaptureSource {
    MelloCaptureMode mode;
    uint32_t         monitor_index;
    void*            hwnd;
    uint32_t         pid;
    /* Process mode only: the caller allows the game capture hook for this
     * game. The client sets it from the catalogue policy and the backend
     * switch; with neither, it stays false and no game is ever hooked.
     * libmello still runs its own run-time checks before it injects.
     * Appended field: existing offsets unchanged. */
    bool             allow_hook;
} MelloCaptureSource;

typedef struct MelloMonitorInfo {
    uint32_t index;
    char     name[128];
    uint32_t width;
    uint32_t height;
    bool     primary;
} MelloMonitorInfo;

/** List connected displays via DXGI. Returns count written. */
MELLO_API int mello_enumerate_monitors(MelloContext* ctx, MelloMonitorInfo* out, int max_count);

typedef struct MelloGameProcess {
    uint32_t pid;
    char     name[128];
    char     exe[260];
    bool     is_fullscreen;
    /* Full executable path (UTF-8); empty when the process has no visible
     * window (the path is only resolved for windowed processes). */
    char     path[520];
    /* Main window title (UTF-8); empty when the process has no visible window. */
    char     title[256];
    bool     is_foreground;
    /* Process creation time, Unix epoch milliseconds; 0 when unavailable
     * (permission denied, or the process exited between enumeration steps).
     * This is when the *game* started, not when Mello noticed it — sessions
     * must be dated from here so a game already running at client launch
     * reports an honest duration. Appended field: existing offsets unchanged. */
    int64_t  started_at_ms;
} MelloGameProcess;

/** List running processes matching the bundled game list. */
MELLO_API int mello_enumerate_games(MelloContext* ctx, MelloGameProcess* out, int max_count);

typedef struct MelloWindow {
    void*    hwnd;
    char     title[256];
    char     exe[256];
    uint32_t pid;
} MelloWindow;

/** List visible top-level windows suitable for capture. Returns count written. */
MELLO_API int mello_enumerate_windows(MelloContext* ctx, MelloWindow* out, int max_count);

/**
 * Capture a thumbnail of a window.
 * Writes RGBA pixels to rgba_out (caller must allocate max_width*max_height*4 bytes).
 * Actual dimensions written to out_width/out_height (may be smaller to preserve aspect ratio).
 * Returns 0 on success, -1 on failure.
 */
MELLO_API int mello_capture_window_thumbnail(
    void* hwnd,
    uint32_t max_width, uint32_t max_height,
    uint8_t* rgba_out, uint32_t* out_width, uint32_t* out_height
);

/* ---- Stream config ---- */

typedef struct MelloStreamConfig {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t bitrate_kbps;
} MelloStreamConfig;

/** Video packet callback: data, size, is_keyframe, timestamp. */
typedef void (*MelloPacketCallback)(void* user_data, const uint8_t* data, int size, bool is_keyframe, uint64_t ts);

/** Audio packet callback: data, size, timestamp. */
typedef void (*MelloAudioPacketCallback)(void* user_data, const uint8_t* data, int size, uint64_t ts);

/** Decoded frame callback: rgba pixels, width, height, timestamp. */
typedef void (*MelloFrameCallback)(void* user_data, const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts);

/** Native decoded frame callback: shared GPU texture handle, width, height, timestamp.
 *  The handle is currently a Windows shared D3D11 texture handle. */
typedef enum MelloNativeFrameFormat {
    MELLO_NATIVE_FRAME_FORMAT_UNKNOWN = 0,
    MELLO_NATIVE_FRAME_FORMAT_RGBA8 = 1,
    MELLO_NATIVE_FRAME_FORMAT_R8_NV12_LAYOUT = 2,
    MELLO_NATIVE_FRAME_FORMAT_NV12 = 3,
} MelloNativeFrameFormat;

/** Native decoded frame callback: shared GPU texture handle + format metadata.
 *  - `w`/`h`: visible video dimensions
 *  - `format`: texture layout
 *  - `uv_y_offset`: row offset where UV plane starts (for NV12-layout formats) */
typedef void (*MelloNativeFrameCallback)(
    void*                   user_data,
    void*                   shared_handle,
    uint32_t                w,
    uint32_t                h,
    MelloNativeFrameFormat  format,
    uint32_t                uv_y_offset,
    uint64_t                ts
);

/* ---- Host ---- */

/** Start hosting with a specific capture source. Returns an opaque handle. */
MELLO_API MelloStreamHost* mello_stream_start_host(
    MelloContext*             ctx,
    const MelloCaptureSource* source,
    const MelloStreamConfig*  config,
    MelloPacketCallback       on_packet,
    void*                     user_data
);

MELLO_API void mello_stream_stop_host(MelloStreamHost* host);

/** Get the actual capture resolution after host pipeline has started. */
MELLO_API void mello_stream_get_host_resolution(MelloStreamHost* host, uint32_t* width, uint32_t* height);

/** Capture pipelines abandoned this session because a capture thread would not
 *  stop (stuck in the display driver). Non-zero means leaked GPU resources. */
MELLO_API uint32_t mello_stream_abandoned_pipelines(void);

MELLO_API void mello_stream_request_keyframe(MelloStreamHost* host);

/** Hot-reconfigure encoder bitrate without restarting the session. */
MELLO_API MelloResult mello_stream_set_bitrate(MelloStreamHost* host, uint32_t bitrate_kbps);

/**
 * Hot-reconfigure the encoded output framerate without restarting capture.
 *
 * Frames are decimated before GPU colour conversion, so both convert and encode
 * cost fall. At a fixed bitrate, halving the framerate doubles the bits per
 * frame — the difference between a starved 60fps picture and a clean 30fps one.
 *
 * Pass 0 (or a value at/above the configured capture rate) to encode every
 * captured frame.
 */
MELLO_API MelloResult mello_stream_set_framerate(MelloStreamHost* host, uint32_t fps);

/** Register callback for game-audio packets. Must be set before mello_stream_start_audio. */
MELLO_API void mello_stream_set_audio_callback(
    MelloStreamHost*          host,
    MelloAudioPacketCallback  callback,
    void*                     user_data
);

/** Start game-audio loopback capture (WASAPI). */
MELLO_API MelloResult mello_stream_start_audio(MelloStreamHost* host);

/** Stop game-audio loopback capture. */
MELLO_API void mello_stream_stop_audio(MelloStreamHost* host);

/* ---- Viewer ---- */

/** Start viewer pipeline. Returns an opaque handle. */
MELLO_API MelloStreamView* mello_stream_start_viewer(
    MelloContext*            ctx,
    const MelloStreamConfig* config,
    MelloFrameCallback       on_frame,
    void*                    user_data
);

MELLO_API void mello_stream_stop_viewer(MelloStreamView* view);

MELLO_API bool mello_stream_feed_packet(MelloStreamView* view, const uint8_t* data, int size, bool is_keyframe);

/** Number of decoded frames waiting in the ring buffer to be presented. */
MELLO_API int mello_stream_viewer_decode_queue_depth(MelloStreamView* view);

/** LUID of the GPU adapter libmello uses for video, as (HighPart << 32) | LowPart.
 *  Returns 0 when no adapter is usable. Safe to call before any stream starts.
 *
 *  A caller that opens the shared texture handles from
 *  MelloNativeFrameCallback must create its own D3D11 device on this adapter.
 *  A shared handle is only valid on the adapter that made it, so a device on
 *  the system default adapter fails every open with E_INVALIDARG. The two are
 *  the same adapter on a single-GPU machine and differ on a laptop that has
 *  both an integrated and a discrete GPU. */
MELLO_API uint64_t mello_video_adapter_luid(void);

/** Read back the latest decoded frame and deliver it via the frame callback.
 *  Call once per display frame after feeding all available packets. */
MELLO_API bool mello_stream_present_frame(MelloStreamView* view);

/** Register callback for native GPU frame handles on viewer side.
 *  When set, the viewer pipeline can bypass CPU readback in mello_stream_present_frame(). */
MELLO_API void mello_stream_set_native_frame_callback(
    MelloStreamView*          view,
    MelloNativeFrameCallback  callback,
    void*                     user_data
);

/** Feed an encoded game-audio packet received from the host for playback. */
MELLO_API MelloResult mello_stream_feed_audio_packet(
    MelloStreamView* view,
    const uint8_t*   data,
    int              size
);

/* ---- Stats ---- */

typedef struct MelloStreamStats {
    uint32_t bitrate_kbps;
    uint32_t fps_actual;
    uint32_t keyframes_sent;
    uint64_t bytes_sent;
    char     encoder_name[32];
    char     decoder_name[32];
    /* Fields below are appended for remote diagnostics; existing offsets are
     * unchanged. `fps_actual` is the *encode* rate — compare it against
     * frames_captured to tell a stalled capture from a stalled encoder, which
     * is otherwise indistinguishable in host telemetry. */
    uint64_t frames_captured;
    uint32_t capture_idle_ms;      /* since last captured frame; 0 when idle host */
    uint32_t encode_queue_depth;
    uint64_t encode_queue_drops;   /* newest-wins evictions: silent frame loss */
    float    convert_ms;
    float    encode_ms;
    char     capture_backend[16];  /* "dxgi" / "wgc" / "sck" */
    char     gpu_name[128];
    /* Rolling mean, not the last sample: encode time swings by an order of
     * magnitude frame to frame. */
    float    encode_ms_mean;
    /* Phase breakdown of the last encode. encode_ms alone cannot separate a
     * busy encoder block from a stalled bitstream readback. */
    float    encode_submit_ms;
    float    encode_wait_ms;
    float    encode_lock_ms;
    /* Quality features given up to hold the frame budget; 0 = full quality. */
    int32_t  encoder_cost_tier;
    /* Frames re-encoded from the last picture because capture delivered
     * nothing new (idle keepalive). A quiet stream, not a dead capture. */
    uint64_t idle_repeat_frames;
    /* 1 when every capture method failed to deliver a first frame (for a game:
     * probably exclusive fullscreen). Silence after frames never sets it. */
    uint32_t capture_failed;
    /* Capture method changes and reasons, "METHOD:reason;..." (truncated). */
    char     capture_history[96];
    /* What the capture is doing, for the streamer's screen and the viewers':
     *   0  capturing
     *   1  waiting, the game is minimized
     *   2  waiting, the game has drawn nothing yet (loading, or the person has
     *      not reached it)
     *   3  failed, with proof that the game is drawing
     * Anything but 0 means the viewer is looking at a still picture and should
     * be told why. It replaces the earlier `target_available` flag, which could
     * only say whether the window was there. Appended field: existing offsets
     * unchanged. */
    uint32_t capture_state;
    /* Cumulative present-to-capture delay histogram: bucket i counts frames
     * delayed [i, i+1) ms; the last bucket counts >= 31 ms. Diff two reads
     * for a window. */
    uint32_t present_delay_hist[32];
    /* The process behind the capture target has exited. Ends the stream: a
     * minimized game only pauses it, a quit game is never coming back. Sticky;
     * 0 for monitor capture, which has no process behind it. Appended field:
     * existing offsets unchanged. */
    uint8_t  target_exited;
} MelloStreamStats;

MELLO_API void mello_stream_get_stats(MelloStreamHost* host, MelloStreamStats* stats);

/* ---- Cursor ---- */

/** Get latest cursor packet from host. Returns packet size, or 0 if no update. */
MELLO_API int mello_stream_get_cursor_packet(MelloStreamHost* host, uint8_t* buf, int buf_size);

/** Apply a received cursor packet on the viewer side. */
MELLO_API MelloResult mello_stream_apply_cursor_packet(MelloStreamView* view, const uint8_t* buf, int size);

typedef struct MelloCursorState {
    int32_t  x;
    int32_t  y;
    bool     visible;
    uint8_t* shape_rgba;
    uint32_t shape_w;
    uint32_t shape_h;
} MelloCursorState;

MELLO_API void mello_stream_get_cursor_state(MelloStreamView* view, MelloCursorState* out);

/* ============================================================================
 * Debug / Diagnostics
 * ============================================================================ */

typedef struct MelloDebugStats {
    float input_level;
    float silero_vad_prob;
    float rnnoise_prob;
    bool  is_speaking;
    bool  is_capturing;
    bool  is_muted;
    bool  is_deafened;
    bool  echo_cancellation_enabled;
    bool  agc_enabled;
    bool  noise_suppression_enabled;
    uint32_t packets_encoded;
    uint32_t aec_capture_frames;
    uint32_t aec_render_frames;
    int32_t  incoming_streams;
    int32_t  underrun_count;
    int32_t  rtp_recv_total;
    float    pipeline_delay_ms;
    /* Receive-path counters, appended for the voice quality gate. Append new
     * fields after these; never reorder. Counters are totals since the
     * context started.
     * Since the NetEQ playout (spec 10 section 5) the jitter buffer and the
     * custom concealment are gone. These fields stay for the ABI. Each one
     * carries its NetEQ equivalent, or 0 where none exists. The full NetEQ
     * statistics follow input_gate_dbfs below. */
    uint32_t rx_frames_decoded;          /* 0: no equivalent */
    uint32_t rx_decode_errors;           /* NetEQ GetAudio failures (decode errors) */
    uint32_t rx_conceal_missing_plc;     /* 0: see rx_concealed_samples */
    uint32_t rx_conceal_gap_fec;         /* 0: see rx_fec_packets_received and _discarded */
    uint32_t rx_conceal_gap_plc;         /* 0: see rx_concealed_samples */
    uint32_t rx_conceal_fill_plc;        /* 0: no fill concealment any more */
    uint32_t rx_jitter_missing;          /* 0: no equivalent */
    uint32_t rx_jitter_dropped_late;     /* = rx_packets_discarded (too late or duplicate) */
    uint32_t rx_jitter_dropped_overflow; /* 0: NetEQ flushes instead, see rx_jitter_resets */
    uint32_t rx_jitter_resets;           /* = rx_stream_resets + rx_buffer_flushes */
    int32_t  rx_jitter_buffered_packets; /* 0: see rx_buffer_ms */
    float    rx_jitter_target_delay_ms;  /* = rx_target_delay_ms */
    float    rx_playout_buffer_ms;       /* = rx_buffer_ms */
    /* Late playout: 10 ms frames in which a peer played voice concealment
     * (NetEQ expand) while packets for it waited in its NetEQ buffer. A talk
     * pause leaves the buffer empty and does not count. underrun_count above
     * counts 10 ms mixes in which no peer produced audio. The windowed voice
     * health reads this counter. */
    uint32_t rx_late_underruns;
    /* Input sensitivity (mello_voice_set_input_sensitivity*): the mode, the
     * manual level, and the gate threshold the last frame was compared with. */
    bool     input_sensitivity_auto;
    float    input_sensitivity_db;
    float    input_gate_dbfs;
    /* NetEQ receive path (spec 10 section 5.4). Delay fields are means over
     * the current remote peers, 0 with no peer. Counters are totals since the
     * context started, over every peer, also peers whose stream ended. */
    float    rx_target_delay_ms;         /* NetEQ TargetDelayMs: the delay it aims for */
    float    rx_filtered_delay_ms;       /* NetEQ FilteredCurrentDelayMs: the buffer delay now */
    float    rx_buffer_ms;               /* NetEQ packet and sync buffer now */
    float    rx_output_ring_ms;          /* mixed audio that waits for the device callback */
    uint64_t rx_total_samples_received;  /* NetEQ lifetime statistics (WebRTC stats spec) */
    uint64_t rx_concealed_samples;
    uint64_t rx_silent_concealed_samples;
    uint64_t rx_concealment_events;
    uint64_t rx_inserted_samples_for_deceleration;
    uint64_t rx_removed_samples_for_acceleration;
    uint64_t rx_fec_packets_received;
    uint64_t rx_fec_packets_discarded;   /* FEC payloads not used (the primary arrived) */
    uint64_t rx_packets_discarded;
    uint64_t rx_jitter_buffer_delay_ms;  /* sum; divide by rx_jitter_buffer_emitted_count */
    uint64_t rx_jitter_buffer_emitted_count;
    uint64_t rx_interruption_count;      /* concealment runs of 150 ms or more */
    uint64_t rx_interruption_duration_ms;
    uint64_t rx_buffer_flushes;          /* NetEQ packet buffer flushes (not the one at a stream's first packet) */
    uint64_t rx_packets_inserted;        /* packets given to NetEQ */
    uint64_t rx_insert_errors;           /* packets NetEQ refused */
    uint64_t rx_packets_rejected;        /* not RTP, or not payload type 111 or 13 */
    uint64_t rx_stream_resets;           /* new SSRC or sender restart: NetEQ rebuilt */
    uint64_t rx_talk_spurts;             /* talk spurts detected (sender speech gate gaps) */
    float    rx_spurt_hold_ms;           /* minimum delay held for the current spurt's pre-roll */
    /* Send leg (spec 10 sections 4.6 and 7.5): the last receiver report about
     * the voice send track given to mello_voice_report_send_feedback in this
     * voice session (one per second from the SFU), and the encoder setting
     * that the adaptation chose. */
    uint32_t tx_rr_reports;              /* reports in this voice session */
    float    tx_rr_fraction_lost_pct;    /* the last report's fraction lost, percent */
    int32_t  tx_rr_cumulative_lost;      /* the last report's cumulative loss */
    float    tx_rr_jitter_ms;            /* the last report's interarrival jitter */
    float    tx_rtt_ms;                  /* round trip from the last report, -1 unknown */
    float    tx_loss_window_pct;         /* loss over the last 2 s, percent; -1 too few packets */
    int32_t  tx_bitrate_kbps;            /* Opus bitrate now */
    int32_t  tx_max_bitrate_kbps;        /* the top rung: the channel bitrate */
    int32_t  tx_packet_loss_perc;        /* OPUS_SET_PACKET_LOSS_PERC now */
    uint32_t tx_rung_changes;            /* ladder steps since the context started */
} MelloDebugStats;

MELLO_API void mello_get_debug_stats(MelloContext* ctx, MelloDebugStats* out);

/* ============================================================================
 * Devices
 * ============================================================================ */

typedef struct MelloDevice {
    const char* id;
    const char* name;
    bool is_default;
} MelloDevice;

/** Get available audio input (capture) devices. Returns count written. */
MELLO_API int mello_get_audio_inputs(MelloContext* ctx, MelloDevice* devices, int max_count);

/** Get available audio output (playback) devices. Returns count written. */
MELLO_API int mello_get_audio_outputs(MelloContext* ctx, MelloDevice* devices, int max_count);

/** Free strings allocated by mello_get_audio_inputs / mello_get_audio_outputs. */
MELLO_API void mello_free_device_list(MelloDevice* devices, int count);

/** Set audio input device. Pass NULL to revert to system default. */
MELLO_API MelloResult mello_set_audio_input(MelloContext* ctx, const char* device_id);

/** Set audio output device. Pass NULL to revert to system default. */
MELLO_API MelloResult mello_set_audio_output(MelloContext* ctx, const char* device_id);

#ifdef __cplusplus
}
#endif

#endif /* MELLO_H */
