/**
 * @file mello.h
 * @brief Mello C API - Audio, Video, and Transport
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
        #define MELLO_API __declspec(dllimport)
    #endif
#else
    #define MELLO_API
#endif

/* ============================================================================
 * Types
 * ============================================================================ */

typedef struct MelloContext MelloContext;

typedef enum MelloResult {
    MELLO_OK = 0,
    MELLO_ERROR_INVALID_PARAM = -1,
    MELLO_ERROR_NOT_INITIALIZED = -2,
    MELLO_ERROR_ALREADY_STARTED = -3,
    MELLO_ERROR_FAILED = -4,
} MelloResult;

/* ============================================================================
 * Context
 * ============================================================================ */

/** Initialize libmello. Call once at startup. */
MELLO_API MelloContext* mello_init(void);

/** Shutdown and free resources. */
MELLO_API void mello_destroy(MelloContext* ctx);

/** Get last error message. */
MELLO_API const char* mello_get_error(MelloContext* ctx);

/* ============================================================================
 * Voice
 * ============================================================================ */

/** Start audio capture from default microphone. */
MELLO_API MelloResult mello_voice_start_capture(MelloContext* ctx);

/** Stop audio capture. */
MELLO_API MelloResult mello_voice_stop_capture(MelloContext* ctx);

/** Set mute state. */
MELLO_API void mello_voice_set_mute(MelloContext* ctx, bool muted);

/** Check if local user is currently speaking (VAD). */
MELLO_API bool mello_voice_is_speaking(MelloContext* ctx);

/* ============================================================================
 * Streaming
 * ============================================================================ */

/** Start hosting a stream. */
MELLO_API MelloResult mello_stream_start_host(MelloContext* ctx);

/** Stop hosting. */
MELLO_API MelloResult mello_stream_stop_host(MelloContext* ctx);

#ifdef __cplusplus
}
#endif

#endif /* MELLO_H */
