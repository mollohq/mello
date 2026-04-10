#include "mello.h"
#include "context.hpp"
#include "transport/peer_connection_impl.hpp"
#include "video/video_pipeline.hpp"
#include "video/encoder_factory.hpp"
#include "video/decoder_factory.hpp"
#include "video/process_enum.hpp"
#include "video/window_thumbnail.hpp"
#include "audio/clip_encoder.hpp"
#include "util/log.hpp"
#include <cstring>
#include <cstdlib>

#ifdef _WIN32
#define mello_stricmp _stricmp
#else
#include <strings.h>
#define mello_stricmp strcasecmp
#endif

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

struct MelloStreamHost {
    mello::Context*          ctx;
    MelloPacketCallback      callback;
    void*                    user_data;
    MelloAudioPacketCallback audio_callback;
    void*                    audio_user_data;
};

struct MelloStreamView {
    mello::Context*          ctx;
    MelloFrameCallback       callback;
    void*                    user_data;
};

extern "C" {

/* ============================================================================
 * Context
 * ============================================================================ */

static void init_log_level() {
    const char* env = std::getenv("MELLO_LOG");
    if (!env) env = std::getenv("RUST_LOG");
    if (!env) return;

    if (mello_stricmp(env, "debug") == 0 || mello_stricmp(env, "trace") == 0)
        mello::set_log_level(mello::LogLevel::Debug);
    else if (mello_stricmp(env, "warn") == 0 || mello_stricmp(env, "warning") == 0)
        mello::set_log_level(mello::LogLevel::Warn);
    else if (mello_stricmp(env, "error") == 0)
        mello::set_log_level(mello::LogLevel::Error);
}

MelloContext* mello_init(void) {
    try {
        init_log_level();
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

void mello_set_log_callback(MelloLogCallback callback, void* user_data) {
    mello::set_log_callback(callback, user_data);
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

void mello_voice_set_echo_cancellation(MelloContext* ctx, bool enabled) {
    try {
        if (ctx) ctx_cast(ctx)->audio().set_echo_cancellation(enabled);
    } catch (...) {}
}

void mello_voice_set_agc(MelloContext* ctx, bool enabled) {
    try {
        if (ctx) ctx_cast(ctx)->audio().set_agc(enabled);
    } catch (...) {}
}

void mello_voice_set_input_volume(MelloContext* ctx, float volume) {
    try {
        if (ctx) ctx_cast(ctx)->audio().set_input_volume(volume);
    } catch (...) {}
}

void mello_voice_set_output_volume(MelloContext* ctx, float volume) {
    try {
        if (ctx) ctx_cast(ctx)->audio().set_output_volume(volume);
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
 * Clip Buffer
 * ============================================================================ */

MelloResult mello_clip_buffer_start(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    try {
        ctx_cast(ctx)->audio().start_clip_buffer();
        return MELLO_OK;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

MelloResult mello_clip_buffer_stop(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    try {
        ctx_cast(ctx)->audio().stop_clip_buffer();
        return MELLO_OK;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

bool mello_clip_buffer_active(MelloContext* ctx) {
    if (!ctx) return false;
    return ctx_cast(ctx)->audio().clip_buffer_active();
}

MelloResult mello_clip_capture(MelloContext* ctx, float seconds, const char* output_path) {
    if (!ctx || seconds <= 0.0f || !output_path) return MELLO_ERROR_INVALID_PARAM;
    try {
        bool ok = ctx_cast(ctx)->audio().clip_capture(seconds, std::string(output_path));
        return ok ? MELLO_OK : MELLO_ERROR_FAILED;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

MelloResult mello_clip_play(MelloContext* ctx, const char* wav_path) {
    if (!ctx || !wav_path) return MELLO_ERROR_INVALID_PARAM;
    try {
        bool ok = ctx_cast(ctx)->audio().play_clip(std::string(wav_path));
        return ok ? MELLO_OK : MELLO_ERROR_FAILED;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

MelloResult mello_clip_play_mp4(MelloContext* ctx, const char* mp4_path) {
    if (!ctx || !mp4_path) return MELLO_ERROR_INVALID_PARAM;
    try {
        bool ok = ctx_cast(ctx)->audio().play_mp4(std::string(mp4_path));
        return ok ? MELLO_OK : MELLO_ERROR_FAILED;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

MelloResult mello_clip_stop_playback(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    try {
        ctx_cast(ctx)->audio().stop_clip_playback();
        return MELLO_OK;
    } catch (...) {
        return MELLO_ERROR_FAILED;
    }
}

MelloResult mello_clip_encode(const char* wav_path, const char* mp4_path, int bitrate) {
    if (!wav_path || !mp4_path) return MELLO_ERROR_INVALID_PARAM;
    try {
        bool ok = mello::audio::encode_wav_to_mp4(
            std::string(wav_path), std::string(mp4_path), bitrate);
        return ok ? MELLO_OK : MELLO_ERROR_FAILED;
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

void mello_peer_set_audio_track_callback(MelloPeerConnection* peer, MelloAudioTrackCallback callback, void* user_data) {
    if (!peer) return;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        pc->set_audio_track_callback(callback, user_data);
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

MelloResult mello_peer_send_audio(MelloPeerConnection* peer, const uint8_t* data, int size) {
    if (!peer || !data || size <= 0) return MELLO_ERROR_INVALID_PARAM;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->send_audio(data, size) ? MELLO_OK : MELLO_ERROR_TRANSPORT_FAILED;
    } catch (...) {
        return MELLO_ERROR_TRANSPORT_FAILED;
    }
}

const char* mello_peer_handle_remote_offer(MelloPeerConnection* peer, const char* offer_sdp) {
    if (!peer || !offer_sdp) return nullptr;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->handle_remote_offer(offer_sdp);
    } catch (...) {
        return nullptr;
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

void mello_peer_send_ping(MelloPeerConnection* peer) {
    if (!peer) return;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        pc->send_ping();
    } catch (...) {}
}

float mello_peer_rtt_ms(MelloPeerConnection* peer) {
    if (!peer) return 0.0f;
    try {
        auto* pc = reinterpret_cast<mello::transport::PeerConnectionImpl*>(peer);
        return pc->rtt_ms();
    } catch (...) {
        return 0.0f;
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
        out->echo_cancellation_enabled = audio.echo_cancellation_enabled();
        out->agc_enabled     = audio.agc_enabled();
        out->noise_suppression_enabled = audio.noise_suppression_enabled();
        out->packets_encoded = audio.packets_encoded();
        out->aec_capture_frames = audio.aec_capture_frames();
        out->aec_render_frames  = audio.aec_render_frames();
        out->incoming_streams = audio.active_streams();
        out->underrun_count  = audio.underrun_count();
        out->rtp_recv_total  = audio.rtp_recv_total();
        out->pipeline_delay_ms = audio.pipeline_delay_ms();
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

/* ============================================================================
 * Video / Streaming
 * ============================================================================ */

int mello_get_encoders(MelloContext* ctx, MelloEncoderBackend* out, int max_count) {
    if (!ctx || !out || max_count <= 0) return 0;
    try {
        auto& video = ctx_cast(ctx)->video();
        if (!video.init_device()) return 0;
        auto names = mello::video::enumerate_encoders(video.device());
        int count = 0;
        for (auto* name : names) {
            if (count >= max_count) break;
            if (strcmp(name, "NVENC") == 0)      out[count++] = MELLO_ENCODER_NVENC;
            else if (strcmp(name, "AMF") == 0)    out[count++] = MELLO_ENCODER_AMF;
            else if (strcmp(name, "QSV-oneVPL") == 0) out[count++] = MELLO_ENCODER_QSV;
        }
        return count;
    } catch (...) { return 0; }
}

int mello_get_decoders(MelloContext* ctx, MelloDecoderBackend* out, int max_count) {
    if (!ctx || !out || max_count <= 0) return 0;
    try {
        auto& video = ctx_cast(ctx)->video();
        if (!video.init_device()) return 0;
        auto names = mello::video::enumerate_decoders(video.device());
        int count = 0;
        for (auto* name : names) {
            if (count >= max_count) break;
            if (strcmp(name, "NVDEC") == 0)          out[count++] = MELLO_DECODER_NVDEC;
            else if (strcmp(name, "AMF-Decode") == 0) out[count++] = MELLO_DECODER_AMF;
            else if (strcmp(name, "D3D11VA") == 0)    out[count++] = MELLO_DECODER_D3D11VA;
            else if (strcmp(name, "OpenH264") == 0)   out[count++] = MELLO_DECODER_OPENH264;
            else if (strcmp(name, "dav1d") == 0)      out[count++] = MELLO_DECODER_DAV1D;
        }
        return count;
    } catch (...) { return 0; }
}

bool mello_encoder_available(MelloContext* ctx) {
    if (!ctx) return false;
    try {
        return ctx_cast(ctx)->video().encoder_available();
    } catch (...) { return false; }
}

int mello_enumerate_monitors(MelloContext* ctx, MelloMonitorInfo* out, int max_count) {
    if (!ctx || !out || max_count <= 0) return 0;
    try {
        auto monitors = mello::video::enumerate_monitors();
        int count = static_cast<int>(monitors.size());
        if (count > max_count) count = max_count;
        for (int i = 0; i < count; ++i) {
            out[i].index   = monitors[i].index;
            out[i].width   = monitors[i].width;
            out[i].height  = monitors[i].height;
            out[i].primary = monitors[i].primary;
            strncpy(out[i].name, monitors[i].name.c_str(), sizeof(out[i].name) - 1);
            out[i].name[sizeof(out[i].name) - 1] = '\0';
        }
        return count;
    } catch (...) { return 0; }
}

int mello_enumerate_games(MelloContext* ctx, MelloGameProcess* out, int max_count) {
    if (!ctx || !out || max_count <= 0) return 0;
    try {
        auto games = mello::video::enumerate_game_processes();
        int count = static_cast<int>(games.size());
        if (count > max_count) count = max_count;
        for (int i = 0; i < count; ++i) {
            out[i].pid = games[i].pid;
            out[i].is_fullscreen = games[i].is_fullscreen;
            strncpy(out[i].name, games[i].name.c_str(), sizeof(out[i].name) - 1);
            out[i].name[sizeof(out[i].name) - 1] = '\0';
            strncpy(out[i].exe, games[i].exe.c_str(), sizeof(out[i].exe) - 1);
            out[i].exe[sizeof(out[i].exe) - 1] = '\0';
        }
        return count;
    } catch (...) { return 0; }
}

int mello_enumerate_windows(MelloContext* ctx, MelloWindow* out, int max_count) {
    if (!ctx || !out || max_count <= 0) return 0;
    try {
        auto windows = mello::video::enumerate_visible_windows();
        int count = static_cast<int>(windows.size());
        if (count > max_count) count = max_count;
        for (int i = 0; i < count; ++i) {
            out[i].hwnd = windows[i].hwnd;
            out[i].pid  = windows[i].pid;
            strncpy(out[i].title, windows[i].title.c_str(), sizeof(out[i].title) - 1);
            out[i].title[sizeof(out[i].title) - 1] = '\0';
            strncpy(out[i].exe, windows[i].exe.c_str(), sizeof(out[i].exe) - 1);
            out[i].exe[sizeof(out[i].exe) - 1] = '\0';
        }
        return count;
    } catch (...) { return 0; }
}

int mello_capture_window_thumbnail(
    void* hwnd,
    uint32_t max_width, uint32_t max_height,
    uint8_t* rgba_out, uint32_t* out_width, uint32_t* out_height)
{
    if (!hwnd || !rgba_out || !out_width || !out_height) return -1;
    try {
        return mello::video::capture_window_thumbnail(
            hwnd, max_width, max_height, rgba_out, out_width, out_height);
    } catch (...) { return -1; }
}

MelloStreamHost* mello_stream_start_host(
    MelloContext*             ctx,
    const MelloCaptureSource* source,
    const MelloStreamConfig*  config,
    MelloPacketCallback       on_packet,
    void*                     user_data)
{
    if (!ctx || !source || !config || !on_packet) return nullptr;
    try {
        auto* c = ctx_cast(ctx);
        mello::video::CaptureSourceDesc desc{};
        switch (source->mode) {
            case MELLO_CAPTURE_MONITOR:
                desc.mode = mello::video::CaptureMode::Monitor;
                desc.monitor_index = source->monitor_index;
                break;
            case MELLO_CAPTURE_WINDOW:
                desc.mode = mello::video::CaptureMode::Window;
                desc.hwnd = source->hwnd;
                break;
            case MELLO_CAPTURE_PROCESS:
                desc.mode = mello::video::CaptureMode::Process;
                desc.pid = source->pid;
                break;
        }

        mello::video::PipelineConfig pc{};
        pc.width        = config->width;
        pc.height       = config->height;
        pc.fps          = config->fps;
        pc.bitrate_kbps = config->bitrate_kbps;
        pc.low_latency  = true;

        auto* host = new MelloStreamHost{c, on_packet, user_data, nullptr, nullptr};

        auto cb = [host](const uint8_t* data, size_t size, bool is_keyframe, uint64_t ts) {
            host->callback(host->user_data, data, static_cast<int>(size), is_keyframe, ts);
        };

        if (!c->video().start_host(desc, pc, cb)) {
            delete host;
            return nullptr;
        }
        return host;
    } catch (...) { return nullptr; }
}

void mello_stream_stop_host(MelloStreamHost* host) {
    if (!host) return;
    try {
        host->ctx->video().stop_host();
        delete host;
    } catch (...) {}
}

void mello_stream_get_host_resolution(MelloStreamHost* host, uint32_t* width, uint32_t* height) {
    if (!host || !width || !height) return;
    host->ctx->video().get_host_resolution(*width, *height);
}

void mello_stream_request_keyframe(MelloStreamHost* host) {
    if (!host) return;
    try { host->ctx->video().request_keyframe(); } catch (...) {}
}

MelloResult mello_stream_set_bitrate(MelloStreamHost* host, uint32_t bitrate_kbps) {
    if (!host) return MELLO_ERROR_INVALID_PARAM;
    try {
        host->ctx->video().set_bitrate(bitrate_kbps);
        return MELLO_OK;
    } catch (...) { return MELLO_ERROR_FAILED; }
}

void mello_stream_set_audio_callback(
    MelloStreamHost* host,
    MelloAudioPacketCallback callback,
    void* user_data)
{
    if (!host) return;
    host->audio_callback = callback;
    host->audio_user_data = user_data;
}

MelloResult mello_stream_start_audio(MelloStreamHost* host) {
    if (!host) return MELLO_ERROR_INVALID_PARAM;
    MELLO_LOG_WARN("stream", "mello_stream_start_audio: stub (loopback not yet implemented)");
    return MELLO_OK;
}

void mello_stream_stop_audio(MelloStreamHost* host) {
    (void)host;
    MELLO_LOG_INFO("stream", "mello_stream_stop_audio: stub");
}

MelloStreamView* mello_stream_start_viewer(
    MelloContext*            ctx,
    const MelloStreamConfig* config,
    MelloFrameCallback       on_frame,
    void*                    user_data)
{
    if (!ctx || !config || !on_frame) return nullptr;
    try {
        auto* c = ctx_cast(ctx);

        mello::video::PipelineConfig pc{};
        pc.width        = config->width;
        pc.height       = config->height;
        pc.fps          = config->fps;
        pc.bitrate_kbps = config->bitrate_kbps;

        auto* view = new MelloStreamView{c, on_frame, user_data};

        auto cb = [view](const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts) {
            view->callback(view->user_data, rgba, w, h, ts);
        };

        if (!c->video().start_viewer(pc, cb)) {
            delete view;
            return nullptr;
        }
        return view;
    } catch (...) { return nullptr; }
}

void mello_stream_stop_viewer(MelloStreamView* view) {
    if (!view) return;
    try {
        view->ctx->video().stop_viewer();
        delete view;
    } catch (...) {}
}

bool mello_stream_feed_packet(MelloStreamView* view, const uint8_t* data, int size, bool is_keyframe) {
    if (!view || !data || size <= 0) return false;
    try {
        return view->ctx->video().feed_packet(data, static_cast<size_t>(size), is_keyframe);
    } catch (...) { return false; }
}

bool mello_stream_present_frame(MelloStreamView* view) {
    if (!view) return false;
    try {
        return view->ctx->video().present_frame();
    } catch (...) { return false; }
}

MelloResult mello_stream_feed_audio_packet(MelloStreamView* view, const uint8_t* data, int size) {
    (void)view; (void)data; (void)size;
    MELLO_LOG_DEBUG("stream", "mello_stream_feed_audio_packet: stub");
    return MELLO_OK;
}

void mello_stream_get_stats(MelloStreamHost* host, MelloStreamStats* stats) {
    if (!host || !stats) return;
    try {
        memset(stats, 0, sizeof(MelloStreamStats));
        mello::video::EncoderStats es{};
        host->ctx->video().get_stats(es);
        stats->bitrate_kbps  = es.bitrate_kbps;
        stats->fps_actual    = es.fps_actual;
        stats->keyframes_sent = es.keyframes_sent;
        stats->bytes_sent    = es.bytes_sent;
    } catch (...) {
        memset(stats, 0, sizeof(MelloStreamStats));
    }
}

int mello_stream_get_cursor_packet(MelloStreamHost* host, uint8_t* buf, int buf_size) {
    if (!host || !buf || buf_size <= 0) return 0;
    try {
        size_t size = static_cast<size_t>(buf_size);
        if (host->ctx->video().get_cursor_packet(buf, &size)) {
            return static_cast<int>(size);
        }
        return 0;
    } catch (...) { return 0; }
}

MelloResult mello_stream_apply_cursor_packet(MelloStreamView* view, const uint8_t* buf, int size) {
    if (!view || !buf || size <= 0) return MELLO_ERROR_INVALID_PARAM;
    try {
        view->ctx->video().apply_cursor_packet(buf, static_cast<size_t>(size));
        return MELLO_OK;
    } catch (...) { return MELLO_ERROR_FAILED; }
}

void mello_stream_get_cursor_state(MelloStreamView* view, MelloCursorState* out) {
    if (!view || !out) return;
    try {
        mello::video::CursorState cs{};
        view->ctx->video().get_cursor_state(cs);
        out->x       = cs.x;
        out->y       = cs.y;
        out->visible = cs.visible;
        out->shape_w = cs.shape_w;
        out->shape_h = cs.shape_h;
        out->shape_rgba = nullptr;
    } catch (...) {
        memset(out, 0, sizeof(MelloCursorState));
    }
}

} // extern "C"
