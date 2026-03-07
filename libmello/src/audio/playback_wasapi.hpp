#pragma once

#ifdef _WIN32
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <thread>
#include <atomic>
#include <cstdint>
#include "../util/ring_buffer.hpp"

namespace mello::audio {

class WasapiPlayback {
public:
    WasapiPlayback();
    ~WasapiPlayback();

    bool initialize(const char* device_id = nullptr);
    bool start();
    void stop();

    // Feed mono 16-bit PCM samples into the playback buffer
    size_t feed(const int16_t* samples, size_t count);

    uint32_t sample_rate() const { return sample_rate_; }

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
