#include "mello.h"
#include "context.hpp"
#include "transport/peer_connection_impl.hpp"
#include "util/log.hpp"
#include <cstring>
#include <cstdlib>

static char* dup_str(const char* s) {
    if (!s) return nullptr;
    size_t len = strlen(s) + 1;
    char* copy = static_cast<char*>(malloc(len));
    if (copy) memcpy(copy, s, len);
    return copy;
}

static mello::Context* ctx_cast(MelloContext* ctx) {
    return reinterpret_cast<mello::Context*>(ctx);
}

extern "C" {

/* ============================================================================
 * Context
 * ============================================================================ */

MelloContext* mello_init(void) {
    try {
        MELLO_LOG_INFO("api", "mello_init()");
        auto* ctx = new mello::Context();
        if (!ctx->initialize()) {
            MELLO_LOG_ERROR("api", "mello_init: context init failed");
            delete ctx;
            return nullptr;
        }
        MELLO_LOG_INFO("api", "mello_init: ok");
        return reinterpret_cast<MelloContext*>(ctx);
    } catch (...) {
        MELLO_LOG_ERROR("api", "mello_init: exception caught");
        return nullptr;
    }
}

void mello_destroy(MelloContext* ctx) {
    try {
        if (ctx) {
            auto* c = ctx_cast(ctx);
            c->shutdown();
            delete c;
        }
    } catch (...) {}
}

const char* mello_get_error(MelloContext* ctx) {
    if (!ctx) return "Context is null";
    try {
        return ctx_cast(ctx)->get_error();
    } catch (...) {
        return "Unknown error";
    }
}

/* ============================================================================
 * Voice
 * ============================================================================ */

MelloResult mello_voice_start_capture(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    try {
        return ctx_cast(ctx)->audio().start_capture() ? MELLO_OK : MELLO_ERROR_FAILED;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

MelloResult mello_voice_stop_capture(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    try {
        ctx_cast(ctx)->audio().stop_capture();
        return MELLO_OK;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

void mello_voice_set_mute(MelloContext* ctx, bool muted) {
    try {
        if (ctx) ctx_cast(ctx)->audio().set_mute(muted);
    } catch (...) {}
}

void mello_voice_set_deafen(MelloContext* ctx, bool deafened) {
    try {
        if (ctx) ctx_cast(ctx)->audio().set_deafen(deafened);
    } catch (...) {}
}

bool mello_voice_is_speaking(MelloContext* ctx) {
    if (!ctx) return false;
    try {
        return ctx_cast(ctx)->audio().is_speaking();
    } catch (...) {
        return false;
    }
}

void mello_voice_set_vad_callback(
    MelloContext* ctx,
    MelloVoiceActivityCallback callback,
    void* user_data)
{
    if (!ctx || !callback) return;
    try {
        ctx_cast(ctx)->audio().set_vad_callback([callback, user_data](bool speaking) {
            callback(user_data, speaking);
        });
    } catch (...) {}
}

float mello_voice_get_input_level(MelloContext* ctx) {
    if (!ctx) return 0.0f;
    try {
        float level = ctx_cast(ctx)->audio().input_level();
        static int call_count = 0;
        if ((++call_count % 50) == 0) {
            MELLO_LOG_DEBUG("api", "get_input_level: %.4f", level);
        }
        return level;
    } catch (...) {
        return 0.0f;
    }
}

int mello_voice_get_packet(MelloContext* ctx, uint8_t* buffer, int buffer_size) {
    if (!ctx || !buffer || buffer_size <= 0) return 0;
    try {
        return ctx_cast(ctx)->audio().get_packet(buffer, buffer_size);
    } catch (...) {
        return 0;
    }
}

MelloResult mello_voice_feed_packet(
    MelloContext* ctx,
    const char* peer_id,
    const uint8_t* data,
    int size)
{
    if (!ctx || !peer_id || !data || size <= 0) return MELLO_ERROR_INVALID_PARAM;
    try {
        ctx_cast(ctx)->audio().feed_packet(peer_id, data, size);
        return MELLO_OK;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

/* ============================================================================
 * P2P Transport
 * ============================================================================ */

MelloPeerConnection* mello_peer_create(MelloContext* ctx, const char* peer_id) {
    if (!ctx || !peer_id) return nullptr;
    try {
        auto* pc = new mello::transport::PeerConnectionImpl(peer_id);
        return reinterpret_cast<MelloPeerConnection*>(pc);
    } catch (...) {
        return nullptr;
    }
}

void mello_peer_destroy(MelloPeerConnection* peer) {
    try {
        if (peer) {
            delete reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        }
    } catch (...) {}
}

void mello_peer_set_ice_servers(MelloPeerConnection* peer, const char** urls, int count) {
    if (!peer) return;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        std::vector<std::string> servers;
        for (int i = 0; i < count; ++i) {
            if (urls && urls[i]) servers.emplace_back(urls[i]);
        }
        pc->set_ice_servers(servers);
    } catch (...) {}
}

const char* mello_peer_create_offer(MelloPeerConnection* peer) {
    if (!peer) return nullptr;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->create_offer();
    } catch (...) {
        return nullptr;
    }
}

const char* mello_peer_create_answer(MelloPeerConnection* peer, const char* offer_sdp) {
    if (!peer || !offer_sdp) return nullptr;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->create_answer(offer_sdp);
    } catch (...) {
        return nullptr;
    }
}

MelloResult mello_peer_set_remote_description(MelloPeerConnection* peer, const char* sdp, bool is_offer) {
    if (!peer || !sdp) return MELLO_ERROR_INVALID_PARAM;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->set_remote_description(sdp, is_offer) ? MELLO_OK : MELLO_ERROR_TRANSPORT_FAILED;
    } catch (...) {
        return MELLO_ERROR_TRANSPORT_FAILED;
    }
}

MelloResult mello_peer_add_ice_candidate(MelloPeerConnection* peer, const MelloIceCandidate* candidate) {
    if (!peer || !candidate || !candidate->candidate) return MELLO_ERROR_INVALID_PARAM;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->add_ice_candidate(candidate->candidate, candidate->sdp_mid ? candidate->sdp_mid : "",
                                     candidate->sdp_mline_index) ? MELLO_OK : MELLO_ERROR_TRANSPORT_FAILED;
    } catch (...) {
        return MELLO_ERROR_TRANSPORT_FAILED;
    }
}

void mello_peer_set_ice_callback(MelloPeerConnection* peer, MelloIceCandidateCallback callback, void* user_data) {
    if (!peer) return;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        pc->set_ice_callback(callback, user_data);
    } catch (...) {}
}

void mello_peer_set_state_callback(MelloPeerConnection* peer, MelloPeerStateCallback callback, void* user_data) {
    if (!peer) return;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        pc->set_state_callback(callback, user_data);
    } catch (...) {}
}

void mello_peer_set_data_callback(MelloPeerConnection* peer, MelloPeerDataCallback callback, void* user_data) {
    if (!peer) return;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        pc->set_data_callback(callback, user_data);
    } catch (...) {}
}

MelloResult mello_peer_send_unreliable(MelloPeerConnection* peer, const uint8_t* data, int size) {
    if (!peer || !data || size <= 0) return MELLO_ERROR_INVALID_PARAM;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->send_unreliable(data, size) ? MELLO_OK : MELLO_ERROR_TRANSPORT_FAILED;
    } catch (...) {
        return MELLO_ERROR_TRANSPORT_FAILED;
    }
}

MelloResult mello_peer_send_reliable(MelloPeerConnection* peer, const uint8_t* data, int size) {
    if (!peer || !data || size <= 0) return MELLO_ERROR_INVALID_PARAM;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->send_reliable(data, size) ? MELLO_OK : MELLO_ERROR_TRANSPORT_FAILED;
    } catch (...) {
        return MELLO_ERROR_TRANSPORT_FAILED;
    }
}

bool mello_peer_is_connected(MelloPeerConnection* peer) {
    if (!peer) return false;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->is_connected();
    } catch (...) {
        return false;
    }
}

int mello_peer_recv(MelloPeerConnection* peer, uint8_t* buffer, int buffer_size) {
    if (!peer || !buffer || buffer_size <= 0) return 0;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->recv(buffer, buffer_size);
    } catch (...) {
        return 0;
    }
}

/* ============================================================================
 * Debug / Diagnostics
 * ============================================================================ */

void mello_get_debug_stats(MelloContext* ctx, MelloDebugStats* out) {
    if (!ctx || !out) return;
    try {
        auto& audio = ctx_cast(ctx)->audio();
        out->input_level     = audio.input_level();
        out->silero_vad_prob = audio.speech_probability();
        out->rnnoise_prob    = audio.rnnoise_probability();
        out->is_speaking     = audio.is_speaking();
        out->is_capturing    = audio.is_capturing();
        out->is_muted        = audio.is_muted();
        out->is_deafened     = audio.is_deafened();
        out->packets_encoded = audio.packets_encoded();
    } catch (...) {
        memset(out, 0, sizeof(MelloDebugStats));
    }
}

/* ============================================================================
 * Devices
 * ============================================================================ */

int mello_get_audio_inputs(MelloContext* ctx, MelloDevice* devices, int max_count) {
    if (!ctx || !devices || max_count <= 0) return 0;
    try {
        auto& enumerator = ctx_cast(ctx)->audio().device_enumerator();
        auto list = enumerator.list_capture_devices();
        int count = static_cast<int>(list.size());
        if (count > max_count) count = max_count;
        for (int i = 0; i < count; ++i) {
            devices[i].id = dup_str(list[i].id.c_str());
            devices[i].name = dup_str(list[i].name.c_str());
            devices[i].is_default = list[i].is_default;
        }
        return count;
    } catch (...) {
        return 0;
    }
}

int mello_get_audio_outputs(MelloContext* ctx, MelloDevice* devices, int max_count) {
    if (!ctx || !devices || max_count <= 0) return 0;
    try {
        auto& enumerator = ctx_cast(ctx)->audio().device_enumerator();
        auto list = enumerator.list_playback_devices();
        int count = static_cast<int>(list.size());
        if (count > max_count) count = max_count;
        for (int i = 0; i < count; ++i) {
            devices[i].id = dup_str(list[i].id.c_str());
            devices[i].name = dup_str(list[i].name.c_str());
            devices[i].is_default = list[i].is_default;
        }
        return count;
    } catch (...) {
        return 0;
    }
}

void mello_free_device_list(MelloDevice* devices, int count) {
    if (!devices) return;
    for (int i = 0; i < count; ++i) {
        free(const_cast<char*>(devices[i].id));
        free(const_cast<char*>(devices[i].name));
        devices[i].id = nullptr;
        devices[i].name = nullptr;
    }
}

MelloResult mello_set_audio_input(MelloContext* ctx, const char* device_id) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    try {
        return ctx_cast(ctx)->audio().set_capture_device(device_id)
            ? MELLO_OK : MELLO_ERROR_FAILED;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

MelloResult mello_set_audio_output(MelloContext* ctx, const char* device_id) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    try {
        return ctx_cast(ctx)->audio().set_playback_device(device_id)
            ? MELLO_OK : MELLO_ERROR_FAILED;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

// ============================================================================
// Streaming stubs — capture/encode pipeline not yet implemented.
// These allow the Rust orchestration layer to link and call through FFI.
// They will be replaced by real implementations in the capture spec.
// ============================================================================

MelloStreamHost* mello_stream_start_host(MelloContext* ctx, const MelloStreamConfig* config) {
    if (!ctx || !config) return nullptr;
    MELLO_LOG_WARN("stream", "mello_stream_start_host: stub (capture not implemented)");
    // Return a non-null sentinel so the caller knows it "succeeded"
    // but no actual capture/encode happens.
    static int sentinel = 0;
    return reinterpret_cast<MelloStreamHost*>(&sentinel);
}

void mello_stream_stop_host(MelloStreamHost* host) {
    (void)host;
    MELLO_LOG_INFO("stream", "mello_stream_stop_host: stub");
}

int mello_stream_get_video_packet(MelloStreamHost* host, uint8_t* buffer, int buffer_size, bool* is_keyframe) {
    (void)host; (void)buffer; (void)buffer_size;
    if (is_keyframe) *is_keyframe = false;
    return 0; // no packets available
}

int mello_stream_get_audio_packet(MelloStreamHost* host, uint8_t* buffer, int buffer_size) {
    (void)host; (void)buffer; (void)buffer_size;
    return 0; // no packets available
}

void mello_stream_request_keyframe(MelloStreamHost* host) {
    (void)host;
    MELLO_LOG_DEBUG("stream", "mello_stream_request_keyframe: stub");
}

MelloResult mello_stream_set_bitrate(MelloStreamHost* host, uint32_t bitrate_kbps) {
    (void)host; (void)bitrate_kbps;
    MELLO_LOG_DEBUG("stream", "mello_stream_set_bitrate(%u): stub", bitrate_kbps);
    return MELLO_OK;
}

MelloResult mello_stream_start_audio(MelloStreamHost* host) {
    (void)host;
    MELLO_LOG_WARN("stream", "mello_stream_start_audio: stub (loopback not implemented)");
    return MELLO_OK;
}

void mello_stream_stop_audio(MelloStreamHost* host) {
    (void)host;
    MELLO_LOG_INFO("stream", "mello_stream_stop_audio: stub");
}

MelloResult mello_stream_feed_audio_packet(MelloStreamView* view, const uint8_t* data, int size) {
    (void)view; (void)data; (void)size;
    MELLO_LOG_DEBUG("stream", "mello_stream_feed_audio_packet: stub");
    return MELLO_OK;
}

} // extern "C"
