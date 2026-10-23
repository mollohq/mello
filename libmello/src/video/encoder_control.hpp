#pragma once
#include "encoder.hpp"

#include <atomic>
#include <cstdint>
#include <mutex>

namespace mello::video {

/// The control path into an encoder while a host pipeline runs.
///
/// The encode thread encodes and changes cost tiers. Other threads (Rust
/// stream threads, the capture thread on a ladder swap) set the bitrate and
/// the framerate and ask for keyframes. No encoder backend is thread safe:
/// NVENC reconfigures from `base_config_`, and every backend writes
/// `force_idr_` and `config_`. So every call into the encoder runs under one
/// mutex here.
///
/// A setter never waits for an encode. It stores the value, then applies it
/// at once when the mutex is free. When an encode holds the mutex, the encode
/// path applies the value before its next encode, or at `apply_pending`. The
/// encode thread calls `apply_pending` on every wake, at least every 100 ms.
/// A setter that waited could hang its thread on an encode that is stuck in
/// the display driver (2026-09-16: 25 minutes).
///
/// Threads: any. Not reentrant: an encoder must not call back into this.
class EncoderControl {
public:
    /// Sets or clears the encoder. The pointer stays owned by the caller and
    /// must outlive every call made while it is set. Pending values are
    /// dropped: a new encoder starts from its own configuration.
    void attach(Encoder* encoder) {
        std::lock_guard<std::mutex> lock(mutex_);
        encoder_ = encoder;
        pending_bitrate_kbps_.store(0, std::memory_order_release);
        pending_fps_.store(0, std::memory_order_release);
        pending_keyframe_.store(false, std::memory_order_release);
    }

#ifdef _WIN32
    bool encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) {
        std::lock_guard<std::mutex> lock(mutex_);
        apply_pending_locked();
        return encoder_ && encoder_->encode(nv12_texture, out);
    }
#elif defined(__APPLE__)
    bool encode(void* cv_pixel_buffer, EncodedPacket& out) {
        std::lock_guard<std::mutex> lock(mutex_);
        apply_pending_locked();
        return encoder_ && encoder_->encode(cv_pixel_buffer, out);
    }
#endif

    /// Gives up one quality feature. Returns true when the configuration
    /// changed. Encode thread.
    bool reduce_cost_tier() {
        std::lock_guard<std::mutex> lock(mutex_);
        apply_pending_locked();
        return encoder_ && encoder_->reduce_cost_tier();
    }

    /// 0 is ignored: no encoder runs at 0 kbps.
    void set_bitrate(uint32_t kbps) {
        if (kbps == 0) return;
        pending_bitrate_kbps_.store(kbps, std::memory_order_release);
        apply_if_free();
    }

    /// 0 is ignored: the pipeline always passes a real rate.
    void set_framerate(uint32_t fps) {
        if (fps == 0) return;
        pending_fps_.store(fps, std::memory_order_release);
        apply_if_free();
    }

    void request_keyframe() {
        pending_keyframe_.store(true, std::memory_order_release);
        apply_if_free();
    }

    /// Changes the encode size: in place when the backend can
    /// (`Encoder::reconfigure_size`), else a full re-initialize on `device`
    /// with `config` (the new size, the current bitrate and frame rate). The
    /// first frame after it is an IDR either way. Returns false when both
    /// fail; the encoder is then unusable. Encode thread only.
    bool resize(const GraphicsDevice& device, const EncoderConfig& config) {
        std::lock_guard<std::mutex> lock(mutex_);
        apply_pending_locked();
        if (!encoder_) return false;
        if (encoder_->reconfigure_size(config.width, config.height)) {
            encoder_->request_keyframe();
            return true;
        }
        encoder_->shutdown();
        if (!encoder_->initialize(device, config)) return false;
        encoder_->request_keyframe();
        return true;
    }

    /// Applies the pending values now. Waits for an encode that runs.
    /// Encode thread only.
    void apply_pending() {
        std::lock_guard<std::mutex> lock(mutex_);
        apply_pending_locked();
    }

    /// True when a value waits for the encoder. For tests and diagnostics.
    bool has_pending() const {
        return pending_bitrate_kbps_.load(std::memory_order_acquire) != 0 ||
               pending_fps_.load(std::memory_order_acquire) != 0 ||
               pending_keyframe_.load(std::memory_order_acquire);
    }

private:
    void apply_if_free() {
        std::unique_lock<std::mutex> lock(mutex_, std::try_to_lock);
        if (lock.owns_lock()) apply_pending_locked();
    }

    // The order is the order of effect: the framerate sets the bit budget per
    // frame for the bitrate, and the keyframe comes last so a reconfigure that
    // also forces an IDR does not give two.
    void apply_pending_locked() {
        if (!encoder_) return;
        const uint32_t fps = pending_fps_.exchange(0, std::memory_order_acq_rel);
        if (fps != 0) encoder_->set_framerate(fps);
        const uint32_t kbps = pending_bitrate_kbps_.exchange(0, std::memory_order_acq_rel);
        if (kbps != 0) encoder_->set_bitrate(kbps);
        if (pending_keyframe_.exchange(false, std::memory_order_acq_rel)) {
            encoder_->request_keyframe();
        }
    }

    std::mutex            mutex_;
    Encoder*              encoder_ = nullptr;
    std::atomic<uint32_t> pending_bitrate_kbps_{0};
    std::atomic<uint32_t> pending_fps_{0};
    std::atomic<bool>     pending_keyframe_{false};
};

} // namespace mello::video
