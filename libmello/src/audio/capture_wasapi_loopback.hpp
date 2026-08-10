#pragma once

#ifdef _WIN32
#include "audio_capture.hpp"
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <thread>
#include <atomic>
#include <vector>

namespace mello::audio {

/// WASAPI loopback capture from the default render (playback) endpoint.
class WasapiLoopbackCapture : public AudioCapture {
public:
    WasapiLoopbackCapture();
    ~WasapiLoopbackCapture() override;

    bool initialize(const char* device_id = nullptr) override;
    bool start(Callback callback) override;
    void stop() override;

    uint32_t sample_rate() const override { return sample_rate_; }
    uint32_t channels() const override { return channels_; }

private:
    void capture_thread();
    bool init_com();

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
