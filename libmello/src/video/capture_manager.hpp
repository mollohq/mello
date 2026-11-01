#pragma once
#include "capture_source.hpp"
#include "graphics_device.hpp"
#include "present_delay.hpp"
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

namespace mello::video {

/// One captured frame, as the capture manager gives it to each consumer.
struct CapturedFrame {
#ifdef _WIN32
    ID3D11Texture2D* texture = nullptr;  ///< valid only during the call
#else
    void* texture = nullptr;             ///< valid only during the call
#endif
    uint64_t capture_us = 0;
    /// Goes up by one each time the capture method changes. A consumer
    /// forces a keyframe on the first frame it encodes with a new value.
    uint32_t swap_generation = 0;
};

/// The capture of one target, given to every consumer that encodes it
/// (plans/GAME-CLIPS.md section 3.5).
///
/// The manager owns the graphics device, the capture source and the
/// present-to-capture histogram. A consumer (the stream host's
/// `VideoPipeline`) makes its preprocessor and encoder on `device()` and
/// holds the manager by `shared_ptr`, so the device lives longer than its
/// encoder. In this version each stream host makes a private manager with
/// one consumer; the same capture for a second consumer comes later.
///
/// Threads:
///  - The source's capture thread calls each consumer's frame callback, in
///    the order of `add_consumer`. The callback may decimate, convert on the
///    GPU into its own ring and enqueue without a wait. It must not wait on
///    the disk, the network or its encoder: a slow consumer stops the
///    capture for every consumer.
///  - The source's audio thread calls each consumer's audio callbacks.
///  - `add_consumer`, `start`, `remove_consumer` and the audio setters run on
///    the owner's control thread, one at a time (`op_mutex_`). Never from
///    inside a callback of the same manager: they wait for that callback.
///  - The forwarded queries (`state`, `get_cursor`, ...) are safe on any
///    thread, as they are on the source.
///
/// Lock order: `op_mutex_`, then a lock inside the source, then `mutex_`.
/// The delivery paths take a source lock (the reference source calls its
/// audio callback under its own mutex), then `mutex_`. Nothing calls into
/// the source while it holds `mutex_`.
class CaptureManager {
public:
    using ConsumerId = uint32_t;  ///< 0 is no consumer
    using FrameCallback = std::function<void(const CapturedFrame& frame)>;

    enum class RemoveResult {
        /// No call to the consumer runs now, and none comes later.
        Removed,
        /// A capture thread is stuck in the display driver or inside the
        /// consumer. The owner must leak the consumer and this manager, as
        /// `VideoPipeline::abandoned` does.
        Abandoned,
    };

    static constexpr size_t kMaxConsumers = 4;
    /// How long `remove_consumer` waits for a call in flight. The bound of
    /// the capture backends' own stop and of the encode thread join.
    static constexpr std::chrono::milliseconds kRemoveDeadline{5000};

    /// Makes the device, resolves the target (a window that cannot carry a
    /// stream becomes its process), makes the source and initializes it.
    /// Null when a step fails. Windows only; null on other platforms.
    static std::shared_ptr<CaptureManager> create(const CaptureSourceDesc& desc);

    /// A manager over `source` on `device`, both owned from here on (the
    /// device's handle is released with the manager). Initializes the
    /// source with `desc`; null when that fails. `create` uses it, and the
    /// tests use it with a fake source and no device.
    static std::shared_ptr<CaptureManager> create_from(GraphicsDevice device,
                                                       std::unique_ptr<CaptureSource> source,
                                                       const CaptureSourceDesc& desc);

    ~CaptureManager();
    CaptureManager(const CaptureManager&) = delete;
    CaptureManager& operator=(const CaptureManager&) = delete;

    const GraphicsDevice& device() const { return device_; }

    /// Adds a consumer. The next frame reaches it. Returns 0 when the
    /// callback is empty, `kMaxConsumers` are attached, or the last consumer
    /// has already left (the manager is used once).
    ConsumerId add_consumer(FrameCallback on_frame);

    /// Starts the source at `target_fps`. False when the source fails to
    /// start or the manager is stopped. A second call changes nothing.
    bool start(uint32_t target_fps);

    /// Takes the consumer out. After a `Removed` return the manager never
    /// calls it again and the owner may free what the callbacks use. The
    /// last consumer stops the source first, as `VideoPipeline::stop_host`
    /// did; a stop that times out returns `Abandoned`. For another consumer
    /// this waits for a call in flight, at most `kRemoveDeadline`.
    RemoveResult remove_consumer(ConsumerId id);

    /// Routes the source's game audio to a consumer (null: stop; it waits
    /// for a call in flight). The manager sets its own callback on the
    /// source only while a consumer has one, so a source with no listener
    /// makes no audio, as before. See `CaptureSource::set_audio_callback`.
    void set_audio_callback(ConsumerId id, CaptureSource::AudioCallback cb);
    /// The same for the timed audio (`CaptureSource::set_timed_audio_callback`).
    void set_timed_audio_callback(ConsumerId id, CaptureSource::TimedAudioCallback cb);

    /// The swap generation of the latest frame, for a consumer's start value.
    uint32_t swap_generation() const { return swap_generation_.load(std::memory_order_acquire); }

    // Forwarded to the source: one value for every consumer.
    uint32_t width() const { return source_->width(); }
    uint32_t height() const { return source_->height(); }
    const char* backend_name() const { return source_->backend_name(); }
    bool get_cursor(CursorData& out) const { return source_->get_cursor(out); }
    CursorMode cursor_mode() const { return source_->cursor_mode(); }
    bool provides_timed_audio() const { return source_->provides_timed_audio(); }
    bool failed() const { return source_->failed(); }
    CaptureState state() const { return source_->state(); }
    bool target_exited() const { return source_->target_exited(); }
    std::string method_history() const { return source_->method_history(); }

    /// True when the source's stop gave up on its thread. The manager then
    /// keeps the source and the device allocated for good.
    bool stop_timed_out() const { return stop_timed_out_.load(std::memory_order_relaxed); }

    /// Copies the present-to-capture histogram (`PresentDelayHistogram::kBuckets`
    /// values).
    void present_delay_snapshot(uint32_t* out) const { present_delay_hist_.snapshot(out); }

    /// The source itself, for a query that only one backend answers (the
    /// reference clip's timing for the streaming gate).
    CaptureSource* source() const { return source_.get(); }

private:
    struct Key {
        explicit Key() = default;
    };

public:
    /// Use `create` or `create_from`.
    CaptureManager(Key, GraphicsDevice device, std::unique_ptr<CaptureSource> source);

private:
    struct Consumer {
        ConsumerId id = 0;
        FrameCallback on_frame;
        // shared_ptr so a delivery keeps the callback it copied while a
        // setter replaces it; the setter then waits for `audio_in_flight`.
        std::shared_ptr<const CaptureSource::AudioCallback> audio;
        std::shared_ptr<const CaptureSource::TimedAudioCallback> timed_audio;
        bool removed = false;
        uint32_t frames_in_flight = 0;
        uint32_t audio_in_flight = 0;
    };

#ifdef _WIN32
    void deliver_frame(ID3D11Texture2D* texture, uint64_t capture_us);
#else
    void deliver_frame(void* texture, uint64_t capture_us);
#endif
    void deliver_audio(const float* samples, uint32_t frames, uint32_t channels,
                       uint32_t sample_rate);
    void deliver_timed_audio(const float* samples, uint32_t frames, uint32_t channels,
                             uint32_t sample_rate, int64_t capture_us);

    /// Under `mutex_`.
    Consumer* find_locked(ConsumerId id) const;
    /// Under `op_mutex_`, not `mutex_`: sets or clears the manager's own
    /// callbacks on the source to match what the consumers want.
    void update_source_audio();
    /// Under `op_mutex_`: waits until no audio call to `consumer` runs.
    void wait_audio_idle(Consumer* consumer);

    GraphicsDevice device_{};
    std::unique_ptr<CaptureSource> source_;
    PresentDelayHistogram present_delay_hist_;

    std::mutex op_mutex_;
    bool started_ = false;            // under op_mutex_
    bool stopped_ = false;            // under op_mutex_: the last consumer left
    bool audio_installed_ = false;    // under op_mutex_
    bool timed_audio_installed_ = false;  // under op_mutex_
    std::atomic<bool> stop_timed_out_{false};

    mutable std::mutex mutex_;
    std::condition_variable idle_cv_;
    std::vector<std::unique_ptr<Consumer>> consumers_;  // under mutex_
    ConsumerId next_id_ = 1;                            // under mutex_

    std::atomic<uint32_t> swap_generation_{0};
};

}  // namespace mello::video
