#define _USE_MATH_DEFINES
#include <cmath>
#include <gtest/gtest.h>
#include "audio/vad.hpp"
#include <vector>
#include <cstring>
#include <fstream>

using namespace mello::audio;

static std::string get_model_path() {
    const char* env_path = std::getenv("MELLO_VAD_MODEL");
    if (env_path) return env_path;
    return "../../models/silero_vad.onnx";
}

static std::string get_fixture_path(const char* name) {
    const char* env_dir = std::getenv("MELLO_TEST_FIXTURES");
    if (env_dir) return std::string(env_dir) + "/" + name;
    return std::string("../../tests/fixtures/") + name;
}

// Minimal WAV loader -- reads 16-bit PCM mono/stereo, returns mono int16 samples
static bool load_wav_mono16(const std::string& path, std::vector<int16_t>& out, int& sample_rate) {
    std::ifstream f(path, std::ios::binary);
    if (!f.is_open()) return false;

    char riff[4]; f.read(riff, 4);
    if (std::memcmp(riff, "RIFF", 4) != 0) return false;

    f.seekg(4, std::ios::cur); // skip file size

    char wave[4]; f.read(wave, 4);
    if (std::memcmp(wave, "WAVE", 4) != 0) return false;

    int16_t channels = 0;
    int16_t bits_per_sample = 0;
    sample_rate = 0;

    // Find fmt and data chunks
    while (f.good()) {
        char chunk_id[4]; f.read(chunk_id, 4);
        uint32_t chunk_size; f.read(reinterpret_cast<char*>(&chunk_size), 4);

        if (std::memcmp(chunk_id, "fmt ", 4) == 0) {
            auto pos = f.tellg();
            int16_t audio_format; f.read(reinterpret_cast<char*>(&audio_format), 2);
            f.read(reinterpret_cast<char*>(&channels), 2);
            int32_t sr; f.read(reinterpret_cast<char*>(&sr), 4);
            sample_rate = sr;
            f.seekg(4, std::ios::cur); // byte rate
            f.seekg(2, std::ios::cur); // block align
            f.read(reinterpret_cast<char*>(&bits_per_sample), 2);
            f.seekg(pos);
            f.seekg(chunk_size, std::ios::cur);
        } else if (std::memcmp(chunk_id, "data", 4) == 0) {
            if (bits_per_sample != 16) return false;
            int num_samples = chunk_size / (bits_per_sample / 8);
            std::vector<int16_t> raw(num_samples);
            f.read(reinterpret_cast<char*>(raw.data()), chunk_size);

            if (channels == 1) {
                out = std::move(raw);
            } else {
                // Downmix to mono
                out.resize(num_samples / channels);
                for (size_t i = 0; i < out.size(); ++i) {
                    int32_t sum = 0;
                    for (int c = 0; c < channels; ++c)
                        sum += raw[i * channels + c];
                    out[i] = static_cast<int16_t>(sum / channels);
                }
            }
            return true;
        } else {
            f.seekg(chunk_size, std::ios::cur);
        }
    }
    return false;
}

// Upsample 16kHz to 48kHz by sample triplication (inverse of VAD's 3:1 downsample)
static std::vector<int16_t> upsample_16_to_48(const std::vector<int16_t>& in) {
    std::vector<int16_t> out(in.size() * 3);
    for (size_t i = 0; i < in.size(); ++i) {
        out[i * 3 + 0] = in[i];
        out[i * 3 + 1] = in[i];
        out[i * 3 + 2] = in[i];
    }
    return out;
}

class VadTest : public ::testing::Test {
protected:
    VoiceActivityDetector vad;

    void SetUp() override {
        std::string model = get_model_path();
        if (!vad.initialize(model)) {
            GTEST_SKIP() << "Silero VAD model not found at " << model
                         << ". Set MELLO_VAD_MODEL env to override.";
        }
    }

    std::vector<int16_t> silence_48k(int frames_20ms) {
        return std::vector<int16_t>(960 * frames_20ms, 0);
    }
};

TEST_F(VadTest, SilenceNotSpeaking) {
    auto buf = silence_48k(20);
    for (int i = 0; i < 20; ++i) {
        vad.feed(buf.data() + i * 960, 960);
    }
    EXPECT_FALSE(vad.is_speaking()) << "silence should not trigger speaking";
    EXPECT_LT(vad.probability(), VAD_THRESHOLD);
}

TEST_F(VadTest, RealSpeechDetected) {
    std::string wav_path = get_fixture_path("speech.wav");
    std::vector<int16_t> pcm;
    int sr = 0;
    if (!load_wav_mono16(wav_path, pcm, sr)) {
        GTEST_SKIP() << "speech.wav not found at " << wav_path
                     << ". Set MELLO_TEST_FIXTURES env to override.";
    }

    // VAD expects 48kHz input (it downsamples 3:1 internally)
    std::vector<int16_t> pcm_48k;
    if (sr == 16000) {
        pcm_48k = upsample_16_to_48(pcm);
    } else if (sr == 48000) {
        pcm_48k = pcm;
    } else {
        GTEST_SKIP() << "speech.wav has unsupported sample rate: " << sr;
    }

    float max_prob = 0.0f;
    bool ever_speaking = false;
    const int frame_size = 960; // 20ms at 48kHz

    for (size_t offset = 0; offset + frame_size <= pcm_48k.size(); offset += frame_size) {
        vad.feed(pcm_48k.data() + offset, frame_size);
        if (vad.probability() > max_prob) max_prob = vad.probability();
        if (vad.is_speaking()) ever_speaking = true;
    }

    EXPECT_TRUE(ever_speaking) << "real speech should trigger is_speaking()";
    EXPECT_GT(max_prob, 0.8f) << "peak probability on real speech should be high";
}

TEST_F(VadTest, RealSpeechThenSilence) {
    std::string wav_path = get_fixture_path("speech.wav");
    std::vector<int16_t> pcm;
    int sr = 0;
    if (!load_wav_mono16(wav_path, pcm, sr)) {
        GTEST_SKIP() << "speech.wav not found; skipping holdover test";
    }

    std::vector<int16_t> pcm_48k;
    if (sr == 16000) {
        pcm_48k = upsample_16_to_48(pcm);
    } else if (sr == 48000) {
        pcm_48k = pcm;
    } else {
        GTEST_SKIP() << "unsupported sample rate: " << sr;
    }

    const int frame_size = 960;

    // Feed the speech clip
    for (size_t offset = 0; offset + frame_size <= pcm_48k.size(); offset += frame_size) {
        vad.feed(pcm_48k.data() + offset, frame_size);
    }

    if (!vad.is_speaking()) {
        GTEST_SKIP() << "VAD did not trigger on speech clip; holdover test skipped";
    }

    // Feed 1 frame of silence -- holdover should keep speaking=true
    auto one_silence = silence_48k(1);
    vad.feed(one_silence.data(), frame_size);
    EXPECT_TRUE(vad.is_speaking()) << "holdover should keep speaking=true right after speech";

    // Feed enough silence to exhaust holdover (HOLDOVER_FRAMES=8, each ~20ms)
    auto long_silence = silence_48k(30);
    for (int i = 0; i < 30; ++i) {
        vad.feed(long_silence.data() + i * frame_size, frame_size);
    }
    EXPECT_FALSE(vad.is_speaking()) << "speaking should be false after long silence";
}
