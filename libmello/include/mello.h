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

typedef void (*MelloVoiceActivityCallback)(void* user_data, bool speaking);
typedef void (*MelloMicPermissionCallback)(void* user_data, bool granted);
typedef void (*MelloIceCandidateCallback)(void* user_data, const MelloIceCandidate* candidate);
typedef void (*MelloPeerStateCallback)(void* user_data, int state);
typedef void (*MelloPeerDataCallback)(void* user_data, const uint8_t* data, int size, bool reliable);

/* ============================================================================
 * Context
 * ============================================================================ */

MELLO_API MelloContext* mello_init(void);
MELLO_API void mello_destroy(MelloContext* ctx);
MELLO_API const char* mello_get_error(MelloContext* ctx);

/** Set a log callback to receive all libmello log output. Pass NULL to revert to stderr. */
MELLO_API void mello_set_log_callback(MelloLogCallback callback, void* user_data);

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
MELLO_API bool mello_voice_is_speaking(MelloContext* ctx);

MELLO_API void mello_voice_set_vad_callback(
    MelloContext* ctx,
    MelloVoiceActivityCallback callback,
    void* user_data
);

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

/* ============================================================================
 * P2P Transport
 * ============================================================================ */

MELLO_API MelloPeerConnection* mello_peer_create(MelloContext* ctx, const char* peer_id);
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

/** Poll next received unreliable packet. Returns bytes copied, 0 if empty. */
MELLO_API int mello_peer_recv(MelloPeerConnection* peer, uint8_t* buffer, int buffer_size);

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
    MELLO_ENCODER_NVENC = 0,
    MELLO_ENCODER_AMF   = 1,
    MELLO_ENCODER_QSV   = 2,
} MelloEncoderBackend;

typedef enum MelloDecoderBackend {
    MELLO_DECODER_NVDEC    = 0,
    MELLO_DECODER_AMF      = 1,
    MELLO_DECODER_D3D11VA  = 2,
    MELLO_DECODER_OPENH264 = 3,
    MELLO_DECODER_DAV1D    = 4,
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

typedef struct MelloGameProcess {
    uint32_t pid;
    char     name[128];
    char     exe[260];
    bool     is_fullscreen;
} MelloGameProcess;

/** List running processes matching the bundled game list. */
MELLO_API int mello_enumerate_games(MelloContext* ctx, MelloGameProcess* out, int max_count);

typedef struct MelloWindow {
    void*    hwnd;
    char     title[256];
    uint32_t pid;
} MelloWindow;

/** List visible top-level windows suitable for capture. Returns count written. */
MELLO_API int mello_enumerate_windows(MelloContext* ctx, MelloWindow* out, int max_count);

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

/** Read back the latest decoded frame and deliver it via the frame callback.
 *  Call once per display frame after feeding all available packets. */
MELLO_API bool mello_stream_present_frame(MelloStreamView* view);

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
    uint32_t packets_encoded;
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
