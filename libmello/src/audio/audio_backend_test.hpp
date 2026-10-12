#pragma once
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include <memory>

namespace mello::audio {

// Device-free audio backend for the voice quality gate
// (mello-sys/tests/voice_gate.rs).
//
// Selected when the environment variable MELLO_AUDIO_BACKEND is "test" at the
// time create_audio_capture() / create_audio_playback() run. It opens no
// device and starts no thread:
// - TestCapture delivers nothing. Feed audio with start_capture_inject().
// - TestPlayback never calls its render source by itself. The caller pulls
//   mixed output with render(), which is the same call a device thread makes.
// Production never sets the variable, so the platform backends run unchanged.

/// True when MELLO_AUDIO_BACKEND=test is set in the environment.
bool test_audio_backend_requested();

class TestCapture : public AudioCapture {
public:
    bool initialize(const char* device_id = nullptr) override;
    bool start(Callback callback) override;
    void stop() override;
    uint32_t sample_rate() const override { return 48000; }
    uint32_t channels() const override { return 1; }
};

class TestPlayback : public AudioPlayback {
public:
    bool initialize(const char* device_id = nullptr) override;
    bool start() override;
    void stop() override;
    size_t feed(const int16_t* samples, size_t count) override;
    uint32_t sample_rate() const override { return 48000; }

    /// Pull `count` mono 48 kHz samples from the render source, as a device
    /// callback does. Zero-fills what the source did not produce. Returns the
    /// number of samples the source produced. Caller thread only.
    size_t render(int16_t* out, size_t count);
};

std::unique_ptr<AudioCapture> create_test_audio_capture();
std::unique_ptr<AudioPlayback> create_test_audio_playback();

} // namespace mello::audio
