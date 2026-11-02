#ifdef _WIN32
#include "shared_game_audio.hpp"
#include "capture_wasapi_loopback.hpp"
#include "../util/log.hpp"
#include <algorithm>

namespace mello::audio {

namespace {
constexpr const char* TAG = "game_audio";
}

SharedGameAudio::SharedGameAudio(Key, uint32_t pid) : pid_(pid) {}

std::shared_ptr<SharedGameAudio> SharedGameAudio::open(uint32_t pid) {
    if (pid == 0) return nullptr;
    auto shared = std::make_shared<SharedGameAudio>(Key{}, pid);
    auto capture = std::make_unique<WasapiLoopbackCapture>();
    // initialize_scoped falls back to endpoint loopback when process loopback
    // fails; that fallback is refused here.
    if (!capture->initialize_scoped(LoopbackScope::IncludeProcessTree, pid) ||
        capture->scope() != LoopbackScope::IncludeProcessTree) {
        MELLO_LOG_WARN(TAG, "no process loopback for pid %u", pid);
        return nullptr;
    }
    SharedGameAudio* raw = shared.get();
    if (!capture->start([raw](const int16_t* s, size_t n) { raw->deliver(s, n); })) {
        MELLO_LOG_WARN(TAG, "process loopback for pid %u did not start", pid);
        return nullptr;
    }
    shared->capture_ = std::move(capture);
    MELLO_LOG_INFO(TAG, "process loopback for pid %u started (%u Hz, %u ch)", pid,
                   shared->capture_->sample_rate(), shared->capture_->channels());
    return shared;
}

std::shared_ptr<SharedGameAudio> SharedGameAudio::for_test(uint32_t pid) {
    return std::make_shared<SharedGameAudio>(Key{}, pid);
}

SharedGameAudio::~SharedGameAudio() {
    // The capture thread stops before the consumer list goes.
    if (capture_) {
        capture_->stop();
        capture_.reset();
        MELLO_LOG_INFO(TAG, "process loopback for pid %u stopped", pid_);
    }
}

SharedGameAudio::ConsumerId SharedGameAudio::add(Callback callback) {
    if (!callback) return 0;
    std::lock_guard<std::mutex> lock(mutex_);
    const ConsumerId id = next_id_++;
    consumers_.emplace_back(id, std::move(callback));
    return id;
}

void SharedGameAudio::remove(ConsumerId id) {
    std::lock_guard<std::mutex> lock(mutex_);
    consumers_.erase(std::remove_if(consumers_.begin(), consumers_.end(),
                                    [id](const auto& c) { return c.first == id; }),
                     consumers_.end());
}

void SharedGameAudio::deliver(const int16_t* samples, size_t count) {
    std::lock_guard<std::mutex> lock(mutex_);
    for (const auto& consumer : consumers_) consumer.second(samples, count);
}

}  // namespace mello::audio
#endif
