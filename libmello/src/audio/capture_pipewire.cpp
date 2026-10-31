#if defined(__linux__)
#include "capture_pipewire.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <poll.h>
#include <sys/eventfd.h>
#include <unistd.h>
#include <vector>

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
    callback_ = std::move(callback);
    ring_.clear();
    dropped_samples_ = 0;

    wake_fd_ = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (wake_fd_ < 0) {
        MELLO_LOG_ERROR("capture", "PipeWire: eventfd failed");
        return false;
    }
    running_ = true;
    capture_thread_ = std::thread([this] { capture_thread_main(); });

    if (!stream_.connect(1, target_, /*rt_process=*/true)) {
        stop_capture_thread();
        return false;
    }
    MELLO_LOG_INFO("capture", "PipeWire: capture started");
    return true;
}

void PipeWireCapture::stop() {
    if (!running_) return;
    // Remove the node first: after this the data thread calls no more.
    stream_.disconnect();
    stop_capture_thread();
    const uint64_t dropped = dropped_samples_.load();
    if (dropped > 0) {
        MELLO_LOG_WARN("capture", "PipeWire: %llu samples dropped (ring full)",
                       static_cast<unsigned long long>(dropped));
    }
    MELLO_LOG_INFO("capture", "PipeWire: capture stopped");
}

void PipeWireCapture::stop_capture_thread() {
    running_ = false;
    if (wake_fd_ >= 0) {
        const uint64_t one = 1;
        (void)!write(wake_fd_, &one, sizeof(one));
    }
    if (capture_thread_.joinable()) capture_thread_.join();
    if (wake_fd_ >= 0) {
        close(wake_fd_);
        wake_fd_ = -1;
    }
}

// PipeWire's real-time data thread: copy and signal only.
void PipeWireCapture::on_process(pw_stream* stream) {
    pw_buffer* b = pw_stream_dequeue_buffer(stream);
    if (!b) return;
    spa_data& d = b->buffer->datas[0];
    if (d.data && d.chunk && running_) {
        const uint32_t offset = std::min(d.chunk->offset, d.maxsize);
        const uint32_t size = std::min(d.chunk->size, d.maxsize - offset);
        const size_t count = size / sizeof(int16_t);
        if (count > 0) {
            const auto* samples = reinterpret_cast<const int16_t*>(
                static_cast<const uint8_t*>(d.data) + offset);
            const size_t written = ring_.write(samples, count);
            if (written < count) dropped_samples_ += count - written;
            const uint64_t one = 1;
            (void)!write(wake_fd_, &one, sizeof(one));
        }
    }
    pw_stream_queue_buffer(stream, b);
}

// The capture thread: waits for the data thread, then runs the callback on
// everything that arrived.
void PipeWireCapture::capture_thread_main() {
    std::vector<int16_t> chunk(8192);
    pollfd pfd{wake_fd_, POLLIN, 0};
    while (running_) {
        // The timeout only bounds a missed wake; the eventfd normally wakes
        // the thread once per PipeWire cycle.
        poll(&pfd, 1, 50);
        uint64_t counter = 0;
        (void)!read(wake_fd_, &counter, sizeof(counter));
        size_t n = 0;
        while (running_ && (n = ring_.read(chunk.data(), chunk.size())) > 0) {
            if (callback_) callback_(chunk.data(), n);
        }
    }
}

} // namespace mello::audio
#endif
