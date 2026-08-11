#pragma once

#ifdef _WIN32
#include "audio_capture.hpp"
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <thread>
#include <atomic>
#include <vector>

namespace mello::audio {

/// What the loopback capture is allowed to hear.
///
/// Endpoint loopback captures the entire system mix. That includes Mello's own
/// voice playback, so a viewer hears their own voice echoed back out of the
/// streamer's speakers — along with notifications, music, and every other app.
/// Windows 10 2004+ can scope loopback to a process tree, which fixes all three.
enum class LoopbackScope {
    /// Whole system mix. Fallback for pre-2004 Windows only.
    Endpoint,
    /// Only the target process tree — the game being streamed.
    IncludeProcessTree,
    /// Everything except the target process tree — used with our own pid when
    /// streaming a monitor, so the desktop mix is captured without our voice.
    ExcludeProcessTree,
};

/// WASAPI loopback capture, either from the default render endpoint or scoped
/// to a process tree.
class WasapiLoopbackCapture : public AudioCapture {
public:
    WasapiLoopbackCapture();
    ~WasapiLoopbackCapture() override;

    bool initialize(const char* device_id = nullptr) override;

    /// Initialize with an explicit scope, falling back to endpoint loopback when
    /// process loopback is unavailable (older Windows) or activation fails.
    /// Returns true if any capture was established; `scope()` reports which.
    bool initialize_scoped(LoopbackScope scope, uint32_t pid);

    LoopbackScope scope() const { return scope_; }

    bool start(Callback callback) override;
    void stop() override;

    uint32_t sample_rate() const override { return sample_rate_; }
    uint32_t channels() const override { return channels_; }

private:
    void capture_thread();
    bool init_com();
    /// Activate the process-loopback virtual device. Returns false on any
    /// Windows that predates the API, leaving the caller to fall back.
    bool activate_process_loopback(uint32_t pid, bool include_tree);
    /// Shared tail of both init paths: event handle, Initialize, capture client.
    bool finish_client_init(const WAVEFORMATEX* fmt);

    LoopbackScope scope_ = LoopbackScope::Endpoint;

    IMMDevice* device_ = nullptr;
    IAudioClient* audio_client_ = nullptr;
    IAudioCaptureClient* capture_client_ = nullptr;
    HANDLE event_ = nullptr;

    uint32_t sample_rate_ = 48000;
    uint32_t channels_ = 2;
    uint32_t buffer_frames_ = 0;
    uint32_t device_sample_rate_ = 48000;
    uint32_t device_channels_ = 2;
    bool device_float_format_ = true;
    uint16_t device_bits_per_sample_ = 32;

    double resample_src_pos_ = 0.0;
    std::vector<float> src_stereo_f32_;
    std::vector<int16_t> resampled_i16_;

    std::thread thread_;
    std::atomic<bool> running_{false};
    Callback callback_;
    bool com_initialized_ = false;
};

} // namespace mello::audio
#endif
