#pragma once

#ifdef _WIN32
#include "audio_capture.hpp"
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <thread>
#include <atomic>

namespace mello::audio {

class WasapiCapture : public AudioCapture {
public:
    WasapiCapture();
    ~WasapiCapture() override;

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
    uint32_t channels_ = 1;
    uint32_t buffer_frames_ = 0;

    std::thread thread_;
    std::atomic<bool> running_{false};
    Callback callback_;
    bool com_initialized_ = false;
};

} // namespace mello::audio
#endif
