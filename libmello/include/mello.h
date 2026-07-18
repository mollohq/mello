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

/** Get next encoded audio packet to send to peers. Returns packet size, or 0 if none. */
MELLO_API int mello_voice_get_packet(MelloContext* ctx, uint8_t* buffer, int buffer_size);

/** Feed an encoded audio packet received from a peer. */
MELLO_API MelloResult mello_voice_feed_packet(
    MelloContext* ctx,
    const char* peer_id,
    const uint8_t* data,
    int size
);

MELLO_API MelloResult mello_voice_start_capture_inject(MelloContext* ctx);
MELLO_API void mello_voice_inject_capture(MelloContext* ctx, const int16_t* samples, int count);
MELLO_API void mello_voice_stop_capture_inject(MelloContext* ctx);

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

MELLO_API void mello_peer_set_audio_track_callback(
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

/** Send raw Opus frame via the RTP audio track. Packetization is automatic. */
MELLO_API MelloResult mello_peer_send_audio(MelloPeerConnection* peer, const uint8_t* data, int size);

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

/** Number of incoming RTP tracks currently wired (recv_track_count). */
MELLO_API int mello_peer_recv_track_count(MelloPeerConnection* peer);

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
    MELLO_DECODER_DAV1D        = 4,
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
} MelloCaptureMode;

typedef struct MelloCaptureSource {
    MelloCaptureMode mode;
    uint32_t         monitor_index;
    void*            hwnd;
    uint32_t         pid;
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

MELLO_API void mello_stream_request_keyframe(MelloStreamHost* host);

/** Hot-reconfigure encoder bitrate without restarting the session. */
MELLO_API MelloResult mello_stream_set_bitrate(MelloStreamHost* host, uint32_t bitrate_kbps);

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
