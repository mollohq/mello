#pragma once

#ifdef _WIN32
#include "audio_playback.hpp"
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <thread>
#include <atomic>
#include "../util/ring_buffer.hpp"

namespace mello::audio {

class WasapiPlayback : public AudioPlayback {
public:
    WasapiPlayback();
    ~WasapiPlayback() override;

    bool initialize(const char* device_id = nullptr) override;
    bool start() override;
    void stop() override;

    size_t feed(const int16_t* samples, size_t count) override;

    uint32_t sample_rate() const override { return sample_rate_; }

private:
    void playback_thread();

    IMMDevice* device_ = nullptr;
    IAudioClient* audio_client_ = nullptr;
    IAudioRenderClient* render_client_ = nullptr;
    HANDLE event_ = nullptr;

    uint32_t sample_rate_ = 48000;
    uint32_t device_channels_ = 2;
    uint32_t buffer_frames_ = 0;

    std::thread thread_;
    std::atomic<bool> running_{false};
    util::RingBuffer<int16_t> ring_{48000}; // ~1 second buffer
};

} // namespace mello::audio
#endif
