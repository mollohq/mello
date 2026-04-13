#pragma once

#ifdef _WIN32
#include "audio_playback.hpp"
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <thread>
#include <atomic>
#include "../util/ring_buffer.hpp"
#include <vector>

namespace mello::audio {

class AudioSessionWin;

class WasapiPlayback : public AudioPlayback {
public:
    WasapiPlayback();
    ~WasapiPlayback() override;

    void set_session(AudioSessionWin* session) { session_win_ = session; }

    bool initialize(const char* device_id = nullptr) override;
    bool start() override;
    void stop() override;

    size_t feed(const int16_t* samples, size_t count) override;

    uint32_t sample_rate() const override { return sample_rate_; }

private:
    void playback_thread();

    AudioSessionWin* session_win_ = nullptr;

    IMMDevice* device_ = nullptr;
    IAudioClient* audio_client_ = nullptr;
    IAudioRenderClient* render_client_ = nullptr;
    HANDLE event_ = nullptr;

    uint32_t sample_rate_ = 48000; // internal contract rate
    uint32_t device_sample_rate_ = 48000;
    uint32_t device_channels_ = 2;
    bool device_float_format_ = true;
    uint16_t device_bits_per_sample_ = 32;
    uint32_t buffer_frames_ = 0;

    // 48k internal stream -> device-rate stream resampler state
    std::vector<float> src_fifo_;
    double src_fifo_pos_ = 0.0;
    std::vector<int16_t> render_src_i16_;

    std::thread thread_;
    std::atomic<bool> running_{false};
    util::RingBuffer<int16_t> ring_{48000}; // ~1 second buffer
};

} // namespace mello::audio
#endif
