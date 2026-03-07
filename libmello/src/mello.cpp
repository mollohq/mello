#include "mello.h"
#include "context.hpp"
#include "transport/peer_connection_impl.hpp"
#include <cstring>

static mello::Context* ctx_cast(MelloContext* ctx) {
    return reinterpret_cast<mello::Context*>(ctx);
}

extern "C" {

/* ============================================================================
 * Context
 * ============================================================================ */

MelloContext* mello_init(void) {
    try {
        auto* ctx = new mello::Context();
        if (!ctx->initialize()) {
            delete ctx;
            return nullptr;
        }
        return reinterpret_cast<MelloContext*>(ctx);
    } catch (...) {
        return nullptr;
    }
}

void mello_destroy(MelloContext* ctx) {
    if (ctx) {
        auto* c = ctx_cast(ctx);
        c->shutdown();
        delete c;
    }
}

const char* mello_get_error(MelloContext* ctx) {
    if (!ctx) return "Context is null";
    return ctx_cast(ctx)->get_error();
}

/* ============================================================================
 * Voice
 * ============================================================================ */

MelloResult mello_voice_start_capture(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    return ctx_cast(ctx)->audio().start_capture() ? MELLO_OK : MELLO_ERROR_FAILED;
}

MelloResult mello_voice_stop_capture(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    ctx_cast(ctx)->audio().stop_capture();
    return MELLO_OK;
}

void mello_voice_set_mute(MelloContext* ctx, bool muted) {
    if (ctx) ctx_cast(ctx)->audio().set_mute(muted);
}

void mello_voice_set_deafen(MelloContext* ctx, bool deafened) {
    if (ctx) ctx_cast(ctx)->audio().set_deafen(deafened);
}

bool mello_voice_is_speaking(MelloContext* ctx) {
    if (!ctx) return false;
    return ctx_cast(ctx)->audio().is_speaking();
}

void mello_voice_set_vad_callback(
    MelloContext* ctx,
    MelloVoiceActivityCallback callback,
    void* user_data)
{
    if (!ctx || !callback) return;
    ctx_cast(ctx)->audio().set_vad_callback([callback, user_data](bool speaking) {
        callback(user_data, speaking);
    });
}

int mello_voice_get_packet(MelloContext* ctx, uint8_t* buffer, int buffer_size) {
    if (!ctx || !buffer || buffer_size <= 0) return 0;
    return ctx_cast(ctx)->audio().get_packet(buffer, buffer_size);
}

MelloResult mello_voice_feed_packet(
    MelloContext* ctx,
    const char* peer_id,
    const uint8_t* data,
    int size)
{
    if (!ctx || !peer_id || !data || size <= 0) return MELLO_ERROR_INVALID_PARAM;
    ctx_cast(ctx)->audio().feed_packet(peer_id, data, size);
    return MELLO_OK;
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
    if (peer) {
        delete reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    }
}

void mello_peer_set_ice_servers(MelloPeerConnection* peer, const char** urls, int count) {
    if (!peer || !urls || count <= 0) return;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    std::vector<std::string> servers;
    for (int i = 0; i < count; ++i) {
        if (urls[i]) servers.emplace_back(urls[i]);
    }
    pc->set_ice_servers(servers);
}

const char* mello_peer_create_offer(MelloPeerConnection* peer) {
    if (!peer) return nullptr;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    return pc->create_offer();
}

const char* mello_peer_create_answer(MelloPeerConnection* peer, const char* offer_sdp) {
    if (!peer || !offer_sdp) return nullptr;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    return pc->create_answer(offer_sdp);
}

MelloResult mello_peer_set_remote_description(MelloPeerConnection* peer, const char* sdp, bool is_offer) {
    if (!peer || !sdp) return MELLO_ERROR_INVALID_PARAM;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    return pc->set_remote_description(sdp, is_offer) ? MELLO_OK : MELLO_ERROR_TRANSPORT_FAILED;
}

MelloResult mello_peer_add_ice_candidate(MelloPeerConnection* peer, const MelloIceCandidate* candidate) {
    if (!peer || !candidate || !candidate->candidate) return MELLO_ERROR_INVALID_PARAM;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    return pc->add_ice_candidate(candidate->candidate, candidate->sdp_mid ? candidate->sdp_mid : "",
                                 candidate->sdp_mline_index) ? MELLO_OK : MELLO_ERROR_TRANSPORT_FAILED;
}

void mello_peer_set_ice_callback(MelloPeerConnection* peer, MelloIceCandidateCallback callback, void* user_data) {
    if (!peer) return;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    pc->set_ice_callback(callback, user_data);
}

void mello_peer_set_state_callback(MelloPeerConnection* peer, MelloPeerStateCallback callback, void* user_data) {
    if (!peer) return;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    pc->set_state_callback(callback, user_data);
}

void mello_peer_set_data_callback(MelloPeerConnection* peer, MelloPeerDataCallback callback, void* user_data) {
    if (!peer) return;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    pc->set_data_callback(callback, user_data);
}

MelloResult mello_peer_send_unreliable(MelloPeerConnection* peer, const uint8_t* data, int size) {
    if (!peer || !data || size <= 0) return MELLO_ERROR_INVALID_PARAM;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    return pc->send_unreliable(data, size) ? MELLO_OK : MELLO_ERROR_TRANSPORT_FAILED;
}

MelloResult mello_peer_send_reliable(MelloPeerConnection* peer, const uint8_t* data, int size) {
    if (!peer || !data || size <= 0) return MELLO_ERROR_INVALID_PARAM;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    return pc->send_reliable(data, size) ? MELLO_OK : MELLO_ERROR_TRANSPORT_FAILED;
}

bool mello_peer_is_connected(MelloPeerConnection* peer) {
    if (!peer) return false;
    auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
    return pc->is_connected();
}

} // extern "C"
