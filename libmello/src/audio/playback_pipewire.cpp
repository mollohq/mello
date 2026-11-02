#if defined(__linux__)
#include "playback_pipewire.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <cstring>

namespace mello::audio {

PipeWirePlayback::~PipeWirePlayback() {
    stop();
    stream_.close();
}

bool PipeWirePlayback::initialize(const char* device_id) {
    target_ = device_id ? device_id : "";
    return stream_.open("m3llo-voice-playback", /*capture=*/false,
                        [this](pw_stream* s) { on_process(s); });
}

bool PipeWirePlayback::start() {
    if (running_) return false;
    running_ = true;
    if (!stream_.connect(input_channels_, target_)) {
        running_ = false;
        return false;
    }
    MELLO_LOG_INFO("playback", "PipeWire: playback started (%u ch)", input_channels_);
    return true;
}

void PipeWirePlayback::stop() {
    if (!running_) return;
    running_ = false;
    stream_.disconnect();
    MELLO_LOG_INFO("playback", "PipeWire: playback stopped");
}

size_t PipeWirePlayback::feed(const int16_t* samples, size_t count) {
    return ring_.write(samples, count);
}

void PipeWirePlayback::on_process(pw_stream* stream) {
    pw_buffer* b = pw_stream_dequeue_buffer(stream);
    if (!b) return;
    spa_data& d = b->buffer->datas[0];
    if (!d.data || !d.chunk) {
        pw_stream_queue_buffer(stream, b);
        return;
    }

    const uint32_t stride = sizeof(int16_t) * input_channels_;
    // Fill one graph cycle, never more than the buffer holds. Filling the
    // whole buffer would queue up to its full size as latency.
    const uint32_t frames = std::min(stream_.cycle_frames(), d.maxsize / stride);
    auto* out = static_cast<int16_t*>(d.data);
    const size_t wanted = static_cast<size_t>(frames) * input_channels_;

    size_t got = 0;
    if (running_) {
        got = render_source_ ? render_source_(out, wanted) : ring_.read(out, wanted);
    }
    if (got < wanted) std::memset(out + got, 0, (wanted - got) * sizeof(int16_t));

    d.chunk->offset = 0;
    d.chunk->stride = static_cast<int32_t>(stride);
    d.chunk->size = frames * stride;
    pw_stream_queue_buffer(stream, b);
}

} // namespace mello::audio
#endif
