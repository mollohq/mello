#include <gtest/gtest.h>
#include "audio/echo_canceller.hpp"
#include <cmath>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <string>
#include <vector>

using namespace mello::audio;

static constexpr int SAMPLE_RATE = 48000;
static constexpr int CHANNELS = 1;
static constexpr int FRAME_SIZE = 960; // 20ms at 48kHz

class EchoCancellerTest : public ::testing::Test {
protected:
    EchoCanceller ec;

    void SetUp() override {
        ASSERT_TRUE(ec.initialize(SAMPLE_RATE, CHANNELS));
    }

    void fill_silence(int16_t* buf, int n) {
        std::memset(buf, 0, n * sizeof(int16_t));
    }

    void fill_tone(int16_t* buf, int n, float freq_hz = 440.0f, int16_t amplitude = 8000) {
        for (int i = 0; i < n; ++i) {
            float t = static_cast<float>(i) / SAMPLE_RATE;
            buf[i] = static_cast<int16_t>(amplitude * std::sin(2.0f * 3.14159265f * freq_hz * t));
        }
    }
};

TEST_F(EchoCancellerTest, InitShutdown) {
    ec.shutdown();
    ASSERT_TRUE(ec.initialize(SAMPLE_RATE, CHANNELS))
        << "re-init after shutdown should succeed";
}

TEST_F(EchoCancellerTest, DefaultsEnabled) {
    EXPECT_TRUE(ec.aec_enabled());
    EXPECT_TRUE(ec.agc_enabled());
}

TEST_F(EchoCancellerTest, ToggleAec) {
    ec.set_aec_enabled(false);
    EXPECT_FALSE(ec.aec_enabled());
    ec.set_aec_enabled(true);
    EXPECT_TRUE(ec.aec_enabled());
}

TEST_F(EchoCancellerTest, ToggleAgc) {
    ec.set_agc_enabled(false);
    EXPECT_FALSE(ec.agc_enabled());
    ec.set_agc_enabled(true);
    EXPECT_TRUE(ec.agc_enabled());
}

TEST_F(EchoCancellerTest, SilencePassthrough) {
    int16_t buf[FRAME_SIZE];
    fill_silence(buf, FRAME_SIZE);

    ec.process_capture(buf, FRAME_SIZE);

    double energy = 0;
    for (int i = 0; i < FRAME_SIZE; ++i)
        energy += static_cast<double>(buf[i]) * buf[i];
    energy = std::sqrt(energy / FRAME_SIZE);

    EXPECT_LT(energy, 100.0) << "silence should remain near-zero after processing";
}

TEST_F(EchoCancellerTest, ProcessRenderDoesNotCrash) {
    int16_t render[FRAME_SIZE];
    fill_tone(render, FRAME_SIZE);

    // Should not crash or error on valid data
    ec.process_render(render, FRAME_SIZE);
}

TEST_F(EchoCancellerTest, ProcessCaptureAfterRender) {
    int16_t render[FRAME_SIZE];
    int16_t capture[FRAME_SIZE];
    fill_tone(render, FRAME_SIZE, 440.0f, 8000);
    fill_tone(capture, FRAME_SIZE, 1000.0f, 4000);

    ec.process_render(render, FRAME_SIZE);
    ec.process_capture(capture, FRAME_SIZE);

    // Just verify it doesn't crash; AEC convergence needs many frames
}

TEST_F(EchoCancellerTest, DisabledPassthrough) {
    int16_t original[FRAME_SIZE];
    int16_t buf[FRAME_SIZE];
    fill_tone(original, FRAME_SIZE);
    std::memcpy(buf, original, sizeof(buf));

    ec.set_aec_enabled(false);
    ec.set_agc_enabled(false);
    ec.process_capture(buf, FRAME_SIZE);

    EXPECT_EQ(std::memcmp(buf, original, sizeof(buf)), 0)
        << "fully disabled should not modify audio";
}

TEST_F(EchoCancellerTest, MultipleFrames) {
    int16_t render[FRAME_SIZE];
    int16_t capture[FRAME_SIZE];
    fill_tone(render, FRAME_SIZE, 440.0f, 8000);

    for (int i = 0; i < 50; ++i) {
        fill_tone(capture, FRAME_SIZE, 1000.0f, 4000);
        ec.process_render(render, FRAME_SIZE);
        ec.process_capture(capture, FRAME_SIZE);
    }
}

// --- ERLE regression harness ---
// Broadband synthetic loopback: far-end white noise, mic = 20 ms-delayed
// attenuated copy. AGC2 off for isolation. Measures steady-state ERLE.
// Tonal signals are NOT used: AEC3 collapses to ~1 dB on tones.

static uint32_t prng_next(uint32_t& s) {
    s = s * 1664525u + 1013904223u;
    return s;
}

static void fill_broadband(int16_t* buf, int n, uint32_t& seed, int16_t amplitude = 8000) {
    for (int i = 0; i < n; ++i) {
        // White noise in [-amplitude, amplitude], deterministic.
        uint32_t r = prng_next(seed);
        int32_t v = static_cast<int32_t>(r % (2u * static_cast<uint32_t>(amplitude))) - amplitude;
        buf[i] = static_cast<int16_t>(v);
    }
}

static double rms_of(const int16_t* buf, int n) {
    double sum = 0.0;
    for (int i = 0; i < n; ++i) {
        double s = buf[i] / 32768.0;
        sum += s * s;
    }
    return std::sqrt(sum / n);
}

TEST_F(EchoCancellerTest, BroadbandLoopbackCancelsEcho) {
    ec.set_agc_enabled(false);
    ec.set_aec_enabled(true);

    constexpr int kWarmupFrames = 150;   // ~3 s convergence
    constexpr int kMeasureFrames = 150;  // ~3 s steady state
    constexpr int kTotal = kWarmupFrames + kMeasureFrames;
    constexpr float kEchoAtten = 0.5f;  // -6 dB echo path

    uint32_t seed = 0x12345678u;
    std::vector<int16_t> far(FRAME_SIZE);
    std::vector<int16_t> prev_far(FRAME_SIZE, 0);
    std::vector<int16_t> mic(FRAME_SIZE);

    double pre_sum_sq = 0.0;
    double post_sum_sq = 0.0;
    int64_t measure_samples = 0;

    for (int f = 0; f < kTotal; ++f) {
        fill_broadband(far.data(), FRAME_SIZE, seed);
        // 20 ms-delayed attenuated echo: mic = previous far-end frame * atten.
        for (int i = 0; i < FRAME_SIZE; ++i) {
            int32_t e = static_cast<int32_t>(prev_far[i] * kEchoAtten);
            mic[i] = static_cast<int16_t>(e);
        }
        ec.process_render(far.data(), FRAME_SIZE);

        if (f >= kWarmupFrames) {
            double pre_rms = rms_of(mic.data(), FRAME_SIZE);
            pre_sum_sq += pre_rms * pre_rms * FRAME_SIZE;
        }
        ec.process_capture(mic.data(), FRAME_SIZE);
        if (f >= kWarmupFrames) {
            double post_rms = rms_of(mic.data(), FRAME_SIZE);
            post_sum_sq += post_rms * post_rms * FRAME_SIZE;
            measure_samples += FRAME_SIZE;
        }
        prev_far = far;
    }

    double pre_rms = std::sqrt(pre_sum_sq / measure_samples);
    double post_rms = std::sqrt(post_sum_sq / measure_samples);
    double erle_db = 20.0 * std::log10(pre_rms / (post_rms + 1e-12));
    printf("[ERLE] broadband loopback (frame-aligned 20 ms): pre=%.6f post=%.6f ERLE=%.2f dB\n",
           pre_rms, post_rms, erle_db);

    // Baseline on vendored v1.3 (M88), ideal 1-frame sync loopback, AGC2 off:
    // ~24.3 dB (macOS arm64, measured 2026-09-04). Field reports near ~13 dB
    // reflect realistic impairments (latency search, reverb, AGC pumping),
    // not this ideal harness. Threshold stays at 10 dB to catch render-feed
    // regressions; re-baseline toward 25 dB once the engine upgrade lands.
    // The MisalignedDelayLoopback test below covers the delay-estimator path
    // where engine generations actually differ.
    EXPECT_GT(erle_db, 10.0) << "pre_rms=" << pre_rms << " post_rms=" << post_rms;
}

TEST_F(EchoCancellerTest, MisalignedDelayLoopbackCancelsEcho) {
    // Same broadband loopback, but the echo path delay (24.4 ms = 1173
    // samples) is NOT a multiple of the 10 ms APM chunk: the delay
    // estimator must find it instead of starting converged. This is the
    // path where Bluetooth latency and missing delay hints hurt, and where
    // newer delay estimation earns its keep. A delay-line models the lag.
    ec.set_agc_enabled(false);
    ec.set_aec_enabled(true);

    constexpr int kDelaySamples = 1173;
    constexpr int kWarmupFrames = 250;   // longer convergence for search
    constexpr int kMeasureFrames = 150;
    constexpr int kTotal = kWarmupFrames + kMeasureFrames;
    constexpr float kEchoAtten = 0.5f;

    uint32_t seed = 0x9E3779B9u;
    std::vector<int16_t> far(FRAME_SIZE);
    std::vector<int16_t> mic(FRAME_SIZE);
    std::vector<int16_t> history(kDelaySamples + FRAME_SIZE, 0);

    double pre_sum_sq = 0.0;
    double post_sum_sq = 0.0;
    int64_t measure_samples = 0;

    for (int f = 0; f < kTotal; ++f) {
        fill_broadband(far.data(), FRAME_SIZE, seed);
        // Push first, then tap: history holds the last
        // (kDelaySamples + FRAME_SIZE) far-end samples, so history[i]
        // lags the current frame by exactly kDelaySamples.
        history.erase(history.begin(), history.begin() + FRAME_SIZE);
        history.insert(history.end(), far.begin(), far.end());
        for (int i = 0; i < FRAME_SIZE; ++i) {
            int32_t e = static_cast<int32_t>(
                history[i] * kEchoAtten);
            mic[i] = static_cast<int16_t>(e);
        }

        ec.process_render(far.data(), FRAME_SIZE);
        if (f >= kWarmupFrames) {
            double pre_rms = rms_of(mic.data(), FRAME_SIZE);
            pre_sum_sq += pre_rms * pre_rms * FRAME_SIZE;
        }
        ec.process_capture(mic.data(), FRAME_SIZE);
        if (f >= kWarmupFrames) {
            double post_rms = rms_of(mic.data(), FRAME_SIZE);
            post_sum_sq += post_rms * post_rms * FRAME_SIZE;
            measure_samples += FRAME_SIZE;
        }
    }

    double pre_rms = std::sqrt(pre_sum_sq / measure_samples);
    double post_rms = std::sqrt(post_sum_sq / measure_samples);
    double erle_db = 20.0 * std::log10(pre_rms / (post_rms + 1e-12));
    printf("[ERLE] broadband loopback (misaligned 24.4 ms): pre=%.6f post=%.6f ERLE=%.2f dB\n",
           pre_rms, post_rms, erle_db);

    // Deliberately weaker than the aligned case: guards the estimator path
    // without overfitting to one engine generation. Measured v2.1 (M131):
    // 22.7 dB vs 23.7 dB aligned. Investigate (don't just lower) if it
    // drops more than ~6 dB under the aligned result on the same build.
    EXPECT_GT(erle_db, 6.0) << "pre_rms=" << pre_rms << " post_rms=" << post_rms;
}

TEST_F(EchoCancellerTest, BlindRunStaysPassthrough) {
    // No process_render calls: AEC has no reference, so capture must pass
    // through (within +/-3 dB). Catches vacuous harness + render-feed bugs
    // from the other side: if THIS fails, AEC attenuates without reference.
    ec.set_agc_enabled(false);
    ec.set_aec_enabled(true);

    uint32_t seed = 0xABCDEF01u;
    std::vector<int16_t> mic(FRAME_SIZE);
    double pre_sum_sq = 0.0, post_sum_sq = 0.0;
    constexpr int kFrames = 100;
    for (int f = 0; f < kFrames; ++f) {
        fill_broadband(mic.data(), FRAME_SIZE, seed);
        double pre = rms_of(mic.data(), FRAME_SIZE);
        pre_sum_sq += pre * pre * FRAME_SIZE;
        ec.process_capture(mic.data(), FRAME_SIZE);
        double post = rms_of(mic.data(), FRAME_SIZE);
        post_sum_sq += post * post * FRAME_SIZE;
    }
    double pre_rms = std::sqrt(pre_sum_sq / (kFrames * FRAME_SIZE));
    double post_rms = std::sqrt(post_sum_sq / (kFrames * FRAME_SIZE));
    double ratio_db = 20.0 * std::log10((post_rms + 1e-12) / (pre_rms + 1e-12));
    EXPECT_GT(ratio_db, -3.0) << "AEC attenuated without reference";
    EXPECT_LT(ratio_db, 3.0) << "AEC amplified without reference";
}

TEST_F(EchoCancellerTest, StreamDelayHintClamped) {
    ec.set_stream_delay_ms(30);
    EXPECT_EQ(ec.stream_delay_ms(), 30);
    ec.set_stream_delay_ms(-5);
    EXPECT_EQ(ec.stream_delay_ms(), 0);
    ec.set_stream_delay_ms(9999);
    EXPECT_EQ(ec.stream_delay_ms(), 500);
}

TEST_F(EchoCancellerTest, RenderAccumulatesSubFrameChunks) {
    // 48 x 100-sample render callbacks = 4800 samples = exactly ten 10 ms
    // APM chunks. Old code dropped every sub-480 tail (0 frames processed);
    // the accumulator must produce 10.
    ec.set_aec_enabled(true);
    uint32_t before = ec.render_frames();
    int16_t chunk[100];
    uint32_t seed = 0x77777777u;
    fill_broadband(chunk, 100, seed);
    for (int i = 0; i < 48; ++i) {
        ec.process_render(chunk, 100);
    }
    EXPECT_EQ(ec.render_frames() - before, 10u);
}

// --- AGC2 residue-pumping reproduction (expected RED before fix) ---
// Field fact (plans/ECHO-CANCELLATION-IMPROVEMENTS.md, AEC-CLIPPING-REPRO.md):
// AGC2 blasts AEC3 residue up to +19 dB during far-end gaps. Mechanism:
// with EC ON, AEC cancels the echo, so AGC2 sees a quiet post-AEC signal
// and ramps its gain up; when the far-end stops, that high gain blasts the
// near-end noise floor -> "clipping". With EC OFF, AGC2 sees the loud
// uncancelled echo during bursts and keeps its gain low, so the SAME gap
// floor stays quiet. That asymmetry is exactly the field report: heavy
// clipping with EC on, audible-echo-but-clean with EC off.
//
// The test drives a burst/gap far-end with a constant quiet near-end floor
// and measures the gap-floor gain with EC on vs EC off. The EC-on excess
// over EC-off is the defect. Render is fed every frame (zeros in gaps) so
// AEC alignment is ideal and the AGC effect is isolated (render-feed
// continuity is a separate test/concern).

struct GapPumpResult {
    double pre_floor_rms;  // injected near-end floor level (mic in, gap frames)
    double gap_post_rms;   // output level in gap frames (after processing)
};

static GapPumpResult run_gap_pump(EchoCanceller& ec, bool aec_enabled) {
    ec.set_agc_enabled(true);
    ec.set_aec_enabled(aec_enabled);

    constexpr int kBurst = 30;             // ~600 ms far-end active
    constexpr int kGap = 20;               // ~400 ms far-end silent
    constexpr int kCycle = kBurst + kGap;
    constexpr int kCycles = 12;
    constexpr int kMeasureFromCycle = 5;   // let AGC settle before measuring
    constexpr int kGapMeasStart = 2;       // skip the 1-frame echo tail
    constexpr int kGapMeasEnd = 12;        // early-gap transient = what users hear
    constexpr float kEchoAtten = 0.5f;     // -6 dB echo path
    constexpr int16_t kFloorAmp = 200;     // ~ -49 dBFS near-end floor (operator silent)
    constexpr int16_t kFarAmp = 8000;

    uint32_t far_seed = 0x1234u;
    uint32_t floor_seed = 0x0000BEEFu;
    std::vector<int16_t> far(FRAME_SIZE);
    std::vector<int16_t> prev_far(FRAME_SIZE, 0);
    std::vector<int16_t> floor(FRAME_SIZE);
    std::vector<int16_t> mic(FRAME_SIZE);

    double pre_sum_sq = 0.0, post_sum_sq = 0.0;
    int64_t meas_samples = 0;

    for (int c = 0; c < kCycles; ++c) {
        for (int k = 0; k < kCycle; ++k) {
            const bool in_burst = k < kBurst;
            if (in_burst) {
                fill_broadband(far.data(), FRAME_SIZE, far_seed, kFarAmp);
            } else {
                std::memset(far.data(), 0, FRAME_SIZE * sizeof(int16_t));
            }

            // Constant low-level near-end floor (advancing seed = stationary
            // noise at fixed RMS), plus a 1-frame-delayed attenuated echo.
            fill_broadband(floor.data(), FRAME_SIZE, floor_seed, kFloorAmp);
            for (int i = 0; i < FRAME_SIZE; ++i) {
                int32_t v = static_cast<int32_t>(floor[i]) +
                            static_cast<int32_t>(prev_far[i] * kEchoAtten);
                if (v > 32767) v = 32767;
                if (v < -32768) v = -32768;
                mic[i] = static_cast<int16_t>(v);
            }

            ec.process_render(far.data(), FRAME_SIZE);

            const int gap_k = k - kBurst;  // >= 0 while in the gap
            const bool measure = c >= kMeasureFromCycle && !in_burst &&
                                 gap_k >= kGapMeasStart && gap_k < kGapMeasEnd;
            if (measure) {
                const double pre = rms_of(mic.data(), FRAME_SIZE);
                pre_sum_sq += pre * pre * FRAME_SIZE;
            }
            ec.process_capture(mic.data(), FRAME_SIZE);
            if (measure) {
                const double post = rms_of(mic.data(), FRAME_SIZE);
                post_sum_sq += post * post * FRAME_SIZE;
                meas_samples += FRAME_SIZE;
            }
            prev_far = far;
        }
    }

    GapPumpResult r{};
    r.pre_floor_rms = std::sqrt(pre_sum_sq / meas_samples);
    r.gap_post_rms = std::sqrt(post_sum_sq / meas_samples);
    return r;
}

TEST_F(EchoCancellerTest, Agc2DoesNotPumpResidueInFarEndGaps) {
    const GapPumpResult on = run_gap_pump(ec, /*aec_enabled=*/true);
    // Fresh APM state for the control run.
    ec.shutdown();
    ASSERT_TRUE(ec.initialize(SAMPLE_RATE, CHANNELS));
    const GapPumpResult off = run_gap_pump(ec, /*aec_enabled=*/false);

    const double gain_on_db =
        20.0 * std::log10((on.gap_post_rms + 1e-12) / (on.pre_floor_rms + 1e-12));
    const double gain_off_db =
        20.0 * std::log10((off.gap_post_rms + 1e-12) / (off.pre_floor_rms + 1e-12));
    const double excess_db = gain_on_db - gain_off_db;
    printf("[AGC-PUMP] gap-floor gain: EC on=%.2f dB, EC off=%.2f dB, excess=%.2f dB "
           "(gap_on=%.6f gap_off=%.6f floor=%.6f)\n",
           gain_on_db, gain_off_db, excess_db, on.gap_post_rms, off.gap_post_rms,
           on.pre_floor_rms);

    // A healthy AGC treats the near-end floor the same whether or not AEC
    // ran. EC-on must not pump the far-end-gap floor far above EC-off.
    // Field pumping was ~+19 dB; the threshold catches that class.
    EXPECT_LT(excess_db, 6.0)
        << "EC-on pumps the far-end-gap floor " << excess_db
        << " dB above EC-off (AGC2 amplifying AEC residue)";
}

// Invariant guard for the pumping fix: steady low-level input must stay near
// unity gain (AGC2 must not newly amplify a stationary floor). Measured on
// current code: -0.27 dB. The fix must not turn this into amplification.
// NOTE: this does NOT prove quiet-talker normalization — AGC2's VAD treats
// synthetic broadband as noise, not speech, so quiet-SPEECH normalization is
// validated at Level 2 with real LibriSpeech (see plans/AEC-CLIPPING-REPRO.md).
TEST_F(EchoCancellerTest, SteadyLowLevelInputNotAmplified) {
    ec.set_aec_enabled(true);
    ec.set_agc_enabled(true);

    constexpr int kWarm = 200;
    constexpr int kMeasure = 100;
    constexpr int16_t kQuietAmp = 1000;  // ~ -35 dBFS RMS broadband

    uint32_t seed = 0x00005151u;
    std::vector<int16_t> mic(FRAME_SIZE);
    double pre_sum_sq = 0.0, post_sum_sq = 0.0;
    int64_t meas_samples = 0;

    for (int f = 0; f < kWarm + kMeasure; ++f) {
        fill_broadband(mic.data(), FRAME_SIZE, seed, kQuietAmp);
        if (f >= kWarm) {
            const double pre = rms_of(mic.data(), FRAME_SIZE);
            pre_sum_sq += pre * pre * FRAME_SIZE;
        }
        ec.process_capture(mic.data(), FRAME_SIZE);
        if (f >= kWarm) {
            const double post = rms_of(mic.data(), FRAME_SIZE);
            post_sum_sq += post * post * FRAME_SIZE;
            meas_samples += FRAME_SIZE;
        }
    }

    const double pre_rms = std::sqrt(pre_sum_sq / meas_samples);
    const double post_rms = std::sqrt(post_sum_sq / meas_samples);
    const double gain_db = 20.0 * std::log10((post_rms + 1e-12) / (pre_rms + 1e-12));
    printf("[AGC-NORM] steady low-level gain: %.2f dB (pre=%.6f post=%.6f)\n",
           gain_db, pre_rms, post_rms);

    EXPECT_LT(gain_db, 3.0) << "AGC2 amplifies a steady low-level floor: " << gain_db << " dB";
}

// --- Level 2: real-speech pumping harness (skips when dataset absent) ---
// The unit tests above use synthetic broadband, which AGC2 treats as noise,
// not speech. This harness drives REAL near-end and far-end speech through a
// talk / far-only cycle: the near-end talks (AGC2 ramps its gain up on real
// speech), then goes silent while the far-end talks (echo present, cancelled).
// The speech-driven pump — gain ramped for near-end speech lingering into the
// following far-end gap — shows up as residue swelling in the far-only phase.
// Data lives outside the repo (tools/voice-test-client/test-data/clean, fetched
// by fetch_dataset.sh), so the test SKIPS in CI. Set MELLO_AEC_DUMP_WAV=1 to
// write before/after WAVs for subjective A/B.

// Reads a 48 kHz mono 16-bit PCM WAV, scanning chunks (LIST/INFO may precede
// data). Returns false on missing file or format mismatch.
static bool read_wav_mono48k(const std::string& path, std::vector<int16_t>& out) {
    std::ifstream f(path, std::ios::binary);
    if (!f) return false;
    char hdr[12];
    f.read(hdr, 12);
    if (!f || std::string(hdr, 4) != "RIFF" || std::string(hdr + 8, 4) != "WAVE") return false;
    uint16_t fmt = 0, ch = 0, bps = 0;
    uint32_t sr = 0;
    while (f) {
        char id[4];
        uint32_t sz = 0;
        f.read(id, 4);
        f.read(reinterpret_cast<char*>(&sz), 4);
        if (!f) break;
        if (std::string(id, 4) == "fmt ") {
            std::vector<char> buf(sz);
            f.read(buf.data(), static_cast<std::streamsize>(sz));
            if (sz >= 16) {
                std::memcpy(&fmt, buf.data(), 2);
                std::memcpy(&ch, buf.data() + 2, 2);
                std::memcpy(&sr, buf.data() + 4, 4);
                std::memcpy(&bps, buf.data() + 14, 2);
            }
        } else if (std::string(id, 4) == "data") {
            if (ch != 1 || bps != 16 || sr != 48000) return false;
            out.resize(sz / 2);
            f.read(reinterpret_cast<char*>(out.data()), static_cast<std::streamsize>(sz));
            return static_cast<bool>(f);
        } else {
            f.seekg(sz + (sz & 1), std::ios::cur);  // chunks are word-aligned
        }
    }
    return false;
}

static void write_wav_mono48k(const std::string& path, const std::vector<int16_t>& pcm) {
    std::ofstream f(path, std::ios::binary);
    if (!f) return;
    const uint32_t data_bytes = static_cast<uint32_t>(pcm.size() * 2);
    const uint32_t riff = 36 + data_bytes, sr = 48000, byte_rate = 48000 * 2, fmt_sz = 16;
    const uint16_t ch = 1, bps = 16, fmt = 1, block = 2;
    f.write("RIFF", 4); f.write(reinterpret_cast<const char*>(&riff), 4); f.write("WAVE", 4);
    f.write("fmt ", 4); f.write(reinterpret_cast<const char*>(&fmt_sz), 4);
    f.write(reinterpret_cast<const char*>(&fmt), 2); f.write(reinterpret_cast<const char*>(&ch), 2);
    f.write(reinterpret_cast<const char*>(&sr), 4);
    f.write(reinterpret_cast<const char*>(&byte_rate), 4);
    f.write(reinterpret_cast<const char*>(&block), 2); f.write(reinterpret_cast<const char*>(&bps), 2);
    f.write("data", 4); f.write(reinterpret_cast<const char*>(&data_bytes), 4);
    f.write(reinterpret_cast<const char*>(pcm.data()), static_cast<std::streamsize>(data_bytes));
}

// Finds the clean-speech dataset dir from the build cwd or MELLO_AEC_SPEECH_DIR.
static std::string find_speech_dir() {
    if (const char* env = std::getenv("MELLO_AEC_SPEECH_DIR")) {
        if (std::ifstream(std::string(env) + "/librispeech_0.wav").good()) return env;
    }
    const char* candidates[] = {
        "../../tools/voice-test-client/test-data/clean",
        "../tools/voice-test-client/test-data/clean",
        "tools/voice-test-client/test-data/clean",
    };
    for (const char* c : candidates) {
        if (std::ifstream(std::string(c) + "/librispeech_0.wav").good()) return c;
    }
    return "";
}

TEST_F(EchoCancellerTest, RealSpeechNoFarEndGapPump) {
    const std::string dir = find_speech_dir();
    if (dir.empty()) GTEST_SKIP() << "speech dataset not found (run fetch_dataset.sh); skipping";

    std::vector<int16_t> near_src, far_src;
    ASSERT_TRUE(read_wav_mono48k(dir + "/librispeech_0.wav", near_src)) << "near WAV load failed";
    ASSERT_TRUE(read_wav_mono48k(dir + "/librispeech_1.wav", far_src)) << "far WAV load failed";
    ASSERT_GT(near_src.size(), static_cast<size_t>(FRAME_SIZE * 100));
    ASSERT_GT(far_src.size(), static_cast<size_t>(FRAME_SIZE * 100));

    ec.set_aec_enabled(true);
    ec.set_agc_enabled(true);

    constexpr int kNearTalk = 80;   // ~1.6 s near-end speech (AGC ramps up)
    constexpr int kFarOnly = 80;    // ~1.6 s far-end only, near-end silent
    constexpr int kCycle = kNearTalk + kFarOnly;
    constexpr int kCycles = 4;
    constexpr float kEchoAtten = 0.5f;
    constexpr int kMeasureFromCycle = 1;  // let the first cycle warm AEC/AGC

    size_t ni = 0, fi = 0;  // wrap indices into the sources
    uint32_t floor_seed = 0x0C0FFEE0u;
    constexpr int16_t kFloorAmp = 200;  // ~ -49 dBFS ever-present mic floor
    std::vector<int16_t> far(FRAME_SIZE), prev_far(FRAME_SIZE, 0), mic(FRAME_SIZE);
    std::vector<int16_t> floor(FRAME_SIZE);
    std::vector<int16_t> out_dump;  // captured near-end-processed output for A/B

    double faronly_sum_sq = 0.0, far_in_sum_sq = 0.0;
    double faronly_peak = 0.0;
    int64_t faronly_samples = 0;

    for (int c = 0; c < kCycles; ++c) {
        for (int k = 0; k < kCycle; ++k) {
            const bool near_talk = k < kNearTalk;

            // ostkatt's mic always carries a low-level floor (room/breath/keys),
            // present whether or not he is talking. The pump is this floor being
            // amplified during far-only windows, so it must never be zero.
            fill_broadband(floor.data(), FRAME_SIZE, floor_seed, kFloorAmp);

            if (near_talk) {
                std::memset(far.data(), 0, FRAME_SIZE * sizeof(int16_t));  // far-end silent
                for (int i = 0; i < FRAME_SIZE; ++i) {
                    int32_t v = static_cast<int32_t>(near_src[(ni + i) % near_src.size()]) + floor[i];
                    if (v > 32767) v = 32767;
                    if (v < -32768) v = -32768;
                    mic[i] = static_cast<int16_t>(v);
                }
                ni += FRAME_SIZE;
            } else {
                for (int i = 0; i < FRAME_SIZE; ++i) far[i] = far_src[(fi + i) % far_src.size()];
                fi += FRAME_SIZE;
                // near-end silent: mic = floor + NONLINEAR echo of far-end.
                // Real loudspeakers distort (soft-clip); the harmonics AEC3
                // cannot model become speech-correlated residue — exactly what
                // an AGC can latch onto and pump. A pure linear echo cancels too
                // cleanly to represent a real speaker path.
                for (int i = 0; i < FRAME_SIZE; ++i) {
                    const float x = prev_far[i] / 32768.0f;
                    const float dist = std::tanh(3.0f * x) / std::tanh(3.0f);  // speaker soft-clip
                    int32_t e = static_cast<int32_t>(dist * kEchoAtten * 32768.0f);
                    int32_t v = static_cast<int32_t>(floor[i]) + e;
                    if (v > 32767) v = 32767;
                    if (v < -32768) v = -32768;
                    mic[i] = static_cast<int16_t>(v);
                }
            }

            ec.process_render(far.data(), FRAME_SIZE);  // fed every frame
            const int far_k = k - kNearTalk;
            const bool measure = c >= kMeasureFromCycle && !near_talk && far_k >= 2;
            double far_in_rms = 0.0;
            if (measure) far_in_rms = rms_of(far.data(), FRAME_SIZE);
            ec.process_capture(mic.data(), FRAME_SIZE);

            if (measure) {
                const double post = rms_of(mic.data(), FRAME_SIZE);
                faronly_sum_sq += post * post * FRAME_SIZE;
                far_in_sum_sq += far_in_rms * far_in_rms * FRAME_SIZE;
                faronly_peak = std::max(faronly_peak, post);
                faronly_samples += FRAME_SIZE;
            }
            out_dump.insert(out_dump.end(), mic.begin(), mic.end());
            prev_far = far;
        }
    }

    const double residue_rms = std::sqrt(faronly_sum_sq / faronly_samples);
    const double far_in_rms = std::sqrt(far_in_sum_sq / faronly_samples);
    const double residue_dbfs = 20.0 * std::log10(residue_rms + 1e-12);
    const double erle_db = 20.0 * std::log10((far_in_rms + 1e-12) / (residue_rms + 1e-12));
    printf("[REAL-SPEECH] far-only residue: rms=%.6f (%.1f dBFS) peak=%.6f, "
           "far_in=%.6f, echo-path ERLE=%.1f dB\n",
           residue_rms, residue_dbfs, faronly_peak, far_in_rms, erle_db);

    if (std::getenv("MELLO_AEC_DUMP_WAV")) {
        write_wav_mono48k("aec_realspeech_out.wav", out_dump);
        printf("[REAL-SPEECH] wrote aec_realspeech_out.wav (%zu samples)\n", out_dump.size());
    }

    // Near-end is silent in the far-only phase, so the echo residue must stay
    // low. A lingering AGC pump would swell it toward speech level. -30 dBFS
    // catches the pumping class while leaving margin for normal AEC residue.
    EXPECT_LT(residue_dbfs, -30.0)
        << "far-only residue pumped to " << residue_dbfs << " dBFS (AGC lingering gain)";
}

