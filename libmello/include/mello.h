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

typedef void (*MelloVoiceActivityCallback)(void* user_data, bool speaking);
typedef void (*MelloIceCandidateCallback)(void* user_data, const MelloIceCandidate* candidate);
typedef void (*MelloPeerStateCallback)(void* user_data, int state);
typedef void (*MelloPeerDataCallback)(void* user_data, const uint8_t* data, int size, bool reliable);

/* ============================================================================
 * Context
 * ============================================================================ */

MELLO_API MelloContext* mello_init(void);
MELLO_API void mello_destroy(MelloContext* ctx);
MELLO_API const char* mello_get_error(MelloContext* ctx);

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
