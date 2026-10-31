#if defined(__linux__)
#include "pipewire_stream.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <cstring>
#include <mutex>

namespace mello::audio::pw {

void ensure_init() {
    static std::once_flag once;
    std::call_once(once, [] { pw_init(nullptr, nullptr); });
}

std::string parse_default_node_name(const char* json) {
    if (!json) return {};
    const std::string s(json);
    const std::string key = "\"name\"";
    size_t pos = s.find(key);
    if (pos == std::string::npos) return {};
    pos = s.find(':', pos + key.size());
    if (pos == std::string::npos) return {};
    pos = s.find('"', pos + 1);
    if (pos == std::string::npos) return {};
    std::string out;
    for (size_t i = pos + 1; i < s.size(); ++i) {
        const char c = s[i];
        if (c == '\\' && i + 1 < s.size()) {
            out.push_back(s[++i]);
            continue;
        }
        if (c == '"') return out;
        out.push_back(c);
    }
    return {}; // unterminated string
}

Stream::~Stream() { close(); }

bool Stream::open(const char* node_name, bool capture, ProcessFn process) {
    ensure_init();
    name_ = node_name;
    capture_ = capture;
    process_ = std::move(process);

    loop_ = pw_thread_loop_new(node_name, nullptr);
    if (!loop_) {
        MELLO_LOG_ERROR("pipewire", "%s: thread loop creation failed", node_name);
        return false;
    }

    pw_properties* props = pw_properties_new(
        PW_KEY_MEDIA_TYPE, "Audio",
        PW_KEY_MEDIA_CATEGORY, capture ? "Capture" : "Playback",
        PW_KEY_MEDIA_ROLE, "Communication",
        PW_KEY_APP_NAME, "m3llo",
        PW_KEY_NODE_NAME, node_name,
        PW_KEY_NODE_LATENCY, "480/48000",
        nullptr);

    events_ = {};
    events_.version = PW_VERSION_STREAM_EVENTS;
    events_.state_changed = &Stream::on_state_changed;
    events_.process = &Stream::on_process;
    events_.io_changed = &Stream::on_io_changed;

    // pw_stream_new_simple makes its own context and core connection; the
    // stream owns them and frees them in pw_stream_destroy.
    stream_ = pw_stream_new_simple(pw_thread_loop_get_loop(loop_), node_name, props,
                                   &events_, this);
    if (!stream_) {
        MELLO_LOG_ERROR("pipewire", "%s: stream creation failed (is PipeWire running?)",
                        node_name);
        pw_thread_loop_destroy(loop_);
        loop_ = nullptr;
        return false;
    }

    if (pw_thread_loop_start(loop_) < 0) {
        MELLO_LOG_ERROR("pipewire", "%s: thread loop start failed", node_name);
        close();
        return false;
    }
    MELLO_LOG_INFO("pipewire", "%s: stream ready (%s)", node_name,
                   capture ? "capture" : "playback");
    return true;
}

bool Stream::connect(uint32_t channels, const std::string& target, bool rt_process) {
    if (!stream_ || !loop_) return false;
    if (channels < 1) channels = 1;

    pw_thread_loop_lock(loop_);

    if (!target.empty()) {
        // target.object routes to a node by name (PipeWire 0.3.64 and later).
        spa_dict_item item = SPA_DICT_ITEM_INIT("target.object", target.c_str());
        spa_dict dict = SPA_DICT_INIT(&item, 1);
        pw_stream_update_properties(stream_, &dict);
    }

    uint8_t buffer[1024];
    spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
    spa_audio_info_raw info{};
    info.format = SPA_AUDIO_FORMAT_S16;
    info.rate = kRate;
    info.channels = channels;
    if (channels == 1) {
        info.position[0] = SPA_AUDIO_CHANNEL_MONO;
    } else {
        info.position[0] = SPA_AUDIO_CHANNEL_FL;
        info.position[1] = SPA_AUDIO_CHANNEL_FR;
    }
    const spa_pod* params[1] = {spa_format_audio_raw_build(&b, SPA_PARAM_EnumFormat, &info)};

    auto flags = static_cast<pw_stream_flags>(PW_STREAM_FLAG_AUTOCONNECT |
                                              PW_STREAM_FLAG_MAP_BUFFERS);
    if (rt_process) flags = static_cast<pw_stream_flags>(flags | PW_STREAM_FLAG_RT_PROCESS);
    state_ = PW_STREAM_STATE_CONNECTING;
    int res = pw_stream_connect(stream_, capture_ ? PW_DIRECTION_INPUT : PW_DIRECTION_OUTPUT,
                                PW_ID_ANY, flags, params, 1);
    if (res < 0) {
        pw_thread_loop_unlock(loop_);
        MELLO_LOG_ERROR("pipewire", "%s: connect failed: %s", name_.c_str(), strerror(-res));
        return false;
    }
    connected_ = true;

    // Wait for the node: PAUSED once linked and negotiated, then STREAMING.
    for (int i = 0; i < 3 && (state_ == PW_STREAM_STATE_CONNECTING ||
                              state_ == PW_STREAM_STATE_UNCONNECTED);
         ++i) {
        pw_thread_loop_timed_wait(loop_, 1);
    }
    const pw_stream_state state = state_;
    pw_thread_loop_unlock(loop_);

    if (state != PW_STREAM_STATE_PAUSED && state != PW_STREAM_STATE_STREAMING) {
        MELLO_LOG_ERROR("pipewire", "%s: not ready after connect (state=%s)", name_.c_str(),
                        pw_stream_state_as_string(state));
        disconnect();
        return false;
    }
    MELLO_LOG_INFO("pipewire", "%s: connected (%u ch, target=%s)", name_.c_str(), channels,
                   target.empty() ? "default" : target.c_str());
    return true;
}

void Stream::disconnect() {
    if (!stream_ || !loop_ || !connected_) return;
    pw_thread_loop_lock(loop_);
    pw_stream_disconnect(stream_);
    position_ = nullptr;
    connected_ = false;
    pw_thread_loop_unlock(loop_);
    MELLO_LOG_INFO("pipewire", "%s: disconnected", name_.c_str());
}

void Stream::close() {
    if (loop_) pw_thread_loop_stop(loop_);
    if (stream_) {
        pw_stream_destroy(stream_);
        stream_ = nullptr;
    }
    if (loop_) {
        pw_thread_loop_destroy(loop_);
        loop_ = nullptr;
    }
    connected_ = false;
    position_ = nullptr;
}

uint32_t Stream::cycle_frames() const {
    if (!position_ || position_->clock.rate.denom == 0) return kRequestedFrames;
    // The graph counts its cycle in graph-rate frames; this stream runs at
    // 48 kHz. Round up so the cycle is never short.
    const uint64_t graph_rate = position_->clock.rate.denom;
    const uint64_t frames = (position_->clock.duration * kRate + graph_rate - 1) / graph_rate;
    return static_cast<uint32_t>(std::clamp<uint64_t>(frames, 1, 8192));
}

void Stream::on_state_changed(void* data, enum pw_stream_state /*old*/,
                              enum pw_stream_state state, const char* error) {
    auto* self = static_cast<Stream*>(data);
    self->state_ = state;
    if (state == PW_STREAM_STATE_ERROR) {
        MELLO_LOG_ERROR("pipewire", "%s: stream error: %s", self->name_.c_str(),
                        error ? error : "unknown");
    } else {
        MELLO_LOG_INFO("pipewire", "%s: state %s", self->name_.c_str(),
                       pw_stream_state_as_string(state));
    }
    pw_thread_loop_signal(self->loop_, false);
}

void Stream::on_process(void* data) {
    auto* self = static_cast<Stream*>(data);
    if (self->process_) self->process_(self->stream_);
}

void Stream::on_io_changed(void* data, uint32_t id, void* area, uint32_t /*size*/) {
    auto* self = static_cast<Stream*>(data);
    if (id == SPA_IO_Position) self->position_ = static_cast<spa_io_position*>(area);
}

} // namespace mello::audio::pw
#endif
