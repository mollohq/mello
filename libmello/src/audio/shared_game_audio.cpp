#include "shared_game_audio.hpp"
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
    shared->capture_ = open_game_audio_capture(pid, shared.get());
    if (!shared->capture_) return nullptr;
    return shared;
}

std::shared_ptr<SharedGameAudio> SharedGameAudio::for_test(uint32_t pid) {
    return std::make_shared<SharedGameAudio>(Key{}, pid);
}

SharedGameAudio::~SharedGameAudio() {
    // The capture thread stops before the consumer list goes.
    if (capture_) {
        capture_.reset();
        MELLO_LOG_INFO(TAG, "game audio capture for pid %u stopped", pid_);
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

void SharedGameAudio::deliver(const int16_t* samples, size_t count, int64_t capture_us) {
    std::lock_guard<std::mutex> lock(mutex_);
    for (const auto& consumer : consumers_) consumer.second(samples, count, capture_us);
}

#if !defined(_WIN32) && !(defined(__APPLE__) && !defined(MELLO_IOS))
std::unique_ptr<GameAudioCapture> open_game_audio_capture(uint32_t pid, SharedGameAudio*) {
    MELLO_LOG_WARN(TAG, "no game audio capture for pid %u on this platform", pid);
    return nullptr;
}
#endif

}  // namespace mello::audio
