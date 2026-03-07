#pragma once

#ifdef _WIN32
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <functional>
#include <thread>
#include <atomic>
#include <cstdint>

namespace mello::audio {

class WasapiCapture {
public:
    using Callback = std::function<void(const int16_t* samples, size_t count)>;

    WasapiCapture();
    ~WasapiCapture();

    bool initialize(const char* device_id = nullptr);
    bool start(Callback callback);
    void stop();

    uint32_t sample_rate() const { return sample_rate_; }
    uint32_t channels() const { return channels_; }

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
