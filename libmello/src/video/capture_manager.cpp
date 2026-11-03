#include "capture_manager.hpp"

#include "../util/log.hpp"
#include <algorithm>
#include <array>

#ifdef __APPLE__
#include <CoreFoundation/CoreFoundation.h>
#endif

namespace mello::video {

static constexpr const char* TAG = "video/capture-manager";

// Marks one call to a consumer as running. The destructor ends it even when
// the callback throws, so a remove never waits for a call that is gone.
class CaptureManagerCall {
public:
    CaptureManagerCall(std::mutex& mutex, std::condition_variable& cv, uint32_t& counter)
        : mutex_(mutex), cv_(cv), counter_(counter) {}
    ~CaptureManagerCall() {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            --counter_;
        }
        cv_.notify_all();
    }
    CaptureManagerCall(const CaptureManagerCall&) = delete;
    CaptureManagerCall& operator=(const CaptureManagerCall&) = delete;

private:
    std::mutex& mutex_;
    std::condition_variable& cv_;
    uint32_t& counter_;
};

std::shared_ptr<CaptureManager> CaptureManager::create(const CaptureSourceDesc& desc) {
#ifdef _WIN32
    GraphicsDevice device = create_d3d11_device();
    if (!device.handle) {
        MELLO_LOG_ERROR(TAG, "no graphics device for the capture");
        return nullptr;
    }
    // The user picks what to stream; this picks how. A window that cannot
    // carry a stream on its own becomes its process here.
    const CaptureSourceDesc resolved = resolve_capture_target(desc);
    return create_from(device, create_capture_source(resolved), resolved);
#else
    (void)desc;
    return nullptr;
#endif
}

std::shared_ptr<CaptureManager> CaptureManager::create_from(GraphicsDevice device,
                                                            std::unique_ptr<CaptureSource> source,
                                                            const CaptureSourceDesc& desc) {
    // The manager owns the device from here, so every failure below frees it.
    auto manager = std::make_shared<CaptureManager>(Key{}, device, std::move(source));
    if (!manager->source_ || !manager->source_->initialize(manager->device_, desc)) {
        return nullptr;
    }
    return manager;
}

CaptureManager::CaptureManager(Key, GraphicsDevice device, std::unique_ptr<CaptureSource> source)
    : device_(device), source_(std::move(source)) {
    // Before initialize, as the stream host did: a backend that the source
    // builds while it initializes gets the histogram too.
    if (source_) source_->set_present_delay_histogram(&present_delay_hist_);
}

CaptureManager::~CaptureManager() {
    bool must_stop = false;
    {
        std::lock_guard<std::mutex> op(op_mutex_);
        must_stop = started_ && !stopped_;
        stopped_ = true;
    }
    // The owner removes every consumer first, and the last remove stops the
    // source. This stop is for an owner that did not.
    if (must_stop && source_) {
        MELLO_LOG_WARN(TAG, "destroyed with consumers attached; stopping %s",
                       source_->backend_name());
        source_->stop();
        if (source_->stop_timed_out()) stop_timed_out_.store(true, std::memory_order_relaxed);
    }
    if (stop_timed_out_.load(std::memory_order_relaxed)) {
        // A capture thread is still inside the source and may use the device.
        // Freeing either is a use after free; leaking them is the cheap end.
        (void)source_.release();
        MELLO_LOG_ERROR(TAG, "the capture source and its device stay allocated (stop timed out)");
        return;
    }
    source_.reset();
    if (device_.handle) {
#ifdef _WIN32
        device_.d3d11()->Release();
#elif defined(__APPLE__)
        CFRelease(device_.handle);
#endif
        device_.handle = nullptr;
    }
}

CaptureManager::ConsumerId CaptureManager::add_consumer(FrameCallback on_frame) {
    if (!on_frame) return 0;
    std::lock_guard<std::mutex> op(op_mutex_);
    if (stopped_) {
        MELLO_LOG_WARN(TAG, "add_consumer: the capture has stopped");
        return 0;
    }
    std::lock_guard<std::mutex> lock(mutex_);
    // An abandoned consumer keeps its slot: the stuck call still counts on it.
    if (consumers_.size() >= kMaxConsumers) {
        MELLO_LOG_WARN(TAG, "add_consumer: %zu consumers attached already", consumers_.size());
        return 0;
    }
    auto consumer = std::make_unique<Consumer>();
    consumer->id = next_id_++;
    consumer->on_frame = std::move(on_frame);
    const ConsumerId id = consumer->id;
    consumers_.push_back(std::move(consumer));
    MELLO_LOG_INFO(TAG, "consumer %u attached to %s (%zu attached)", id, source_->backend_name(),
                   consumers_.size());
    return id;
}

bool CaptureManager::start(uint32_t target_fps) {
    std::lock_guard<std::mutex> op(op_mutex_);
    if (stopped_) return false;
    if (started_) return true;
#ifdef _WIN32
    auto on_frame = [this](ID3D11Texture2D* texture, uint64_t capture_us) {
        deliver_frame(texture, capture_us);
    };
#else
    auto on_frame = [this](void* texture, uint64_t capture_us) {
        deliver_frame(texture, capture_us);
    };
#endif
    if (!source_->start(target_fps, on_frame)) return false;
    started_ = true;
    return true;
}

CaptureManager::RemoveResult CaptureManager::remove_consumer(ConsumerId id) {
    std::lock_guard<std::mutex> op(op_mutex_);
    Consumer* consumer = nullptr;
    bool last = false;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        consumer = find_locked(id);
        if (!consumer || consumer->removed) return RemoveResult::Removed;
        // No delivery starts a call to it after this.
        consumer->removed = true;
        last = std::none_of(consumers_.begin(), consumers_.end(),
                            [](const std::unique_ptr<Consumer>& c) { return !c->removed; });
    }
    const auto deadline = std::chrono::steady_clock::now() + kRemoveDeadline;

    if (last && !stopped_) {
        stopped_ = true;
        if (started_) {
            // The backend's stop joins the capture thread with its own 5 s
            // bound. After a stop that returns in time no call runs.
            source_->stop();
            if (source_->stop_timed_out()) {
                stop_timed_out_.store(true, std::memory_order_relaxed);
                MELLO_LOG_ERROR(TAG,
                    "consumer %u: %s could not be stopped; the consumer and the manager "
                    "must stay allocated", id, source_->backend_name());
                return RemoveResult::Abandoned;
            }
        }
    }

    // A source with no listener left makes no audio.
    update_source_audio();

    std::unique_lock<std::mutex> lock(mutex_);
    const bool idle = idle_cv_.wait_until(lock, deadline, [consumer] {
        return consumer->frames_in_flight == 0 && consumer->audio_in_flight == 0;
    });
    if (!idle) {
        // The capture thread is stuck inside this consumer. The entry stays,
        // because that call still ends on its counter.
        MELLO_LOG_ERROR(TAG,
            "consumer %u: a capture call did not return within %lld ms; the consumer "
            "must stay allocated", id, static_cast<long long>(kRemoveDeadline.count()));
        return RemoveResult::Abandoned;
    }
    consumers_.erase(std::remove_if(consumers_.begin(), consumers_.end(),
                                    [consumer](const std::unique_ptr<Consumer>& c) {
                                        return c.get() == consumer;
                                    }),
                     consumers_.end());
    MELLO_LOG_INFO(TAG, "consumer %u removed (%zu attached)", id, consumers_.size());
    return RemoveResult::Removed;
}

void CaptureManager::set_audio_callback(ConsumerId id, CaptureSource::AudioCallback cb) {
    std::lock_guard<std::mutex> op(op_mutex_);
    Consumer* consumer = nullptr;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        consumer = find_locked(id);
        if (!consumer || consumer->removed) return;
        consumer->audio =
            cb ? std::make_shared<const CaptureSource::AudioCallback>(std::move(cb)) : nullptr;
    }
    update_source_audio();
    // A delivery can still run the callback it copied before the change.
    wait_audio_idle(consumer);
}

void CaptureManager::set_timed_audio_callback(ConsumerId id,
                                              CaptureSource::TimedAudioCallback cb) {
    std::lock_guard<std::mutex> op(op_mutex_);
    Consumer* consumer = nullptr;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        consumer = find_locked(id);
        if (!consumer || consumer->removed) return;
        consumer->timed_audio =
            cb ? std::make_shared<const CaptureSource::TimedAudioCallback>(std::move(cb))
               : nullptr;
    }
    update_source_audio();
    wait_audio_idle(consumer);
}

CaptureManager::Consumer* CaptureManager::find_locked(ConsumerId id) const {
    for (const auto& c : consumers_) {
        if (c->id == id) return c.get();
    }
    return nullptr;
}

void CaptureManager::update_source_audio() {
    bool want_audio = false;
    bool want_timed = false;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        for (const auto& c : consumers_) {
            if (c->removed) continue;
            want_audio = want_audio || c->audio != nullptr;
            want_timed = want_timed || c->timed_audio != nullptr;
        }
    }
    // Outside `mutex_`: the source's setter waits for its own audio call,
    // and that call takes `mutex_` in the delivery.
    if (want_audio != audio_installed_) {
        if (want_audio) {
            source_->set_audio_callback(
                [this](const float* samples, uint32_t frames, uint32_t channels,
                       uint32_t sample_rate) {
                    deliver_audio(samples, frames, channels, sample_rate);
                });
        } else {
            source_->set_audio_callback(nullptr);
        }
        audio_installed_ = want_audio;
    }
    if (want_timed != timed_audio_installed_) {
        if (want_timed) {
            source_->set_timed_audio_callback(
                [this](const float* samples, uint32_t frames, uint32_t channels,
                       uint32_t sample_rate, int64_t capture_us) {
                    deliver_timed_audio(samples, frames, channels, sample_rate, capture_us);
                });
        } else {
            source_->set_timed_audio_callback(nullptr);
        }
        timed_audio_installed_ = want_timed;
    }
}

void CaptureManager::wait_audio_idle(Consumer* consumer) {
    std::unique_lock<std::mutex> lock(mutex_);
    if (!idle_cv_.wait_for(lock, kRemoveDeadline,
                           [consumer] { return consumer->audio_in_flight == 0; })) {
        MELLO_LOG_ERROR(TAG, "consumer %u: an audio call did not return within %lld ms",
                        consumer->id, static_cast<long long>(kRemoveDeadline.count()));
    }
}

#ifdef _WIN32
void CaptureManager::deliver_frame(ID3D11Texture2D* texture, uint64_t capture_us) {
#else
void CaptureManager::deliver_frame(void* texture, uint64_t capture_us) {
#endif
    // The source's swap flag is consumed once here, so every consumer sees
    // the change through the generation, whatever frames it decimates.
    if (source_->consume_swap_event()) {
        swap_generation_.fetch_add(1, std::memory_order_acq_rel);
    }
    CapturedFrame frame{};
    frame.texture = texture;
    frame.capture_us = capture_us;
    frame.swap_generation = swap_generation_.load(std::memory_order_acquire);
    frame.target_only = source_->frames_show_only_target();

    // The check of `removed` and the count go under one lock, so a remove
    // cannot pass between them. The calls run outside it.
    std::array<Consumer*, kMaxConsumers> batch{};
    size_t n = 0;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        for (const auto& c : consumers_) {
            if (c->removed || n == batch.size()) continue;
            ++c->frames_in_flight;
            batch[n++] = c.get();
        }
    }
    for (size_t i = 0; i < n; ++i) {
        CaptureManagerCall call(mutex_, idle_cv_, batch[i]->frames_in_flight);
        batch[i]->on_frame(frame);
    }
}

void CaptureManager::deliver_audio(const float* samples, uint32_t frames, uint32_t channels,
                                   uint32_t sample_rate) {
    struct Call {
        Consumer* consumer = nullptr;
        std::shared_ptr<const CaptureSource::AudioCallback> cb;
    };
    std::array<Call, kMaxConsumers> batch{};
    size_t n = 0;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        for (const auto& c : consumers_) {
            if (c->removed || !c->audio || n == batch.size()) continue;
            ++c->audio_in_flight;
            batch[n++] = {c.get(), c->audio};
        }
    }
    for (size_t i = 0; i < n; ++i) {
        CaptureManagerCall call(mutex_, idle_cv_, batch[i].consumer->audio_in_flight);
        (*batch[i].cb)(samples, frames, channels, sample_rate);
    }
}

void CaptureManager::deliver_timed_audio(const float* samples, uint32_t frames,
                                         uint32_t channels, uint32_t sample_rate,
                                         int64_t capture_us) {
    struct Call {
        Consumer* consumer = nullptr;
        std::shared_ptr<const CaptureSource::TimedAudioCallback> cb;
    };
    std::array<Call, kMaxConsumers> batch{};
    size_t n = 0;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        for (const auto& c : consumers_) {
            if (c->removed || !c->timed_audio || n == batch.size()) continue;
            ++c->audio_in_flight;
            batch[n++] = {c.get(), c->timed_audio};
        }
    }
    for (size_t i = 0; i < n; ++i) {
        CaptureManagerCall call(mutex_, idle_cv_, batch[i].consumer->audio_in_flight);
        (*batch[i].cb)(samples, frames, channels, sample_rate, capture_us);
    }
}

}  // namespace mello::video
