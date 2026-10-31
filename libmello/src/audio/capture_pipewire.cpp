#if defined(__linux__)
#include "capture_pipewire.hpp"
#include "../util/log.hpp"
#include <algorithm>

namespace mello::audio {

PipeWireCapture::~PipeWireCapture() {
    stop();
    stream_.close();
}

bool PipeWireCapture::initialize(const char* device_id) {
    target_ = device_id ? device_id : "";
    return stream_.open("m3llo-voice-capture", /*capture=*/true,
                        [this](pw_stream* s) { on_process(s); });
}

bool PipeWireCapture::start(Callback callback) {
    if (running_) return false;
    // Set before connect: the first process call may come before connect()
    // returns.
    callback_ = std::move(callback);
    running_ = true;
    if (!stream_.connect(1, target_)) {
        running_ = false;
        return false;
    }
    MELLO_LOG_INFO("capture", "PipeWire: capture started");
    return true;
}

void PipeWireCapture::stop() {
    if (!running_) return;
    running_ = false;
    // Takes the loop lock, so a process call that runs now finishes first.
    stream_.disconnect();
    MELLO_LOG_INFO("capture", "PipeWire: capture stopped");
}

void PipeWireCapture::on_process(pw_stream* stream) {
    pw_buffer* b = pw_stream_dequeue_buffer(stream);
    if (!b) return;
    spa_data& d = b->buffer->datas[0];
    if (d.data && d.chunk && running_ && callback_) {
        const uint32_t offset = std::min(d.chunk->offset, d.maxsize);
        const uint32_t size = std::min(d.chunk->size, d.maxsize - offset);
        const auto* samples =
            reinterpret_cast<const int16_t*>(static_cast<const uint8_t*>(d.data) + offset);
        if (size >= sizeof(int16_t)) callback_(samples, size / sizeof(int16_t));
    }
    pw_stream_queue_buffer(stream, b);
}

} // namespace mello::audio
#endif
