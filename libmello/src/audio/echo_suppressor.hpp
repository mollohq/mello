#pragma once
// Neural residual-echo suppressor: Microsoft AEC-Challenge 2022 GRU
// baseline via PINTO0309's ONNX conversion (Apache-2.0), running one
// inference per 20 ms frame after AEC3 on the software path.
//
// Pipeline fit: 960 int16 @48 kHz in -> FIR decimate /3 -> 320 float @16 kHz
// -> sqrt-Hann + rDFT-320 front-end -> mask inference against the far-end
// reference ring -> irDFT -> FIR interpolate x3 -> 960 @48 kHz out, in place.
// GRU states persist across frames; reset on capture start, device switch,
// and enable. Load failure is soft: process() becomes a passthrough and the
// pipeline logs once, never blocks audio on model availability.
//
// Budgets: <=10 ms inference per frame, <=20 MB RAM, <=6 MB installer.
// The model file is 5.0 MB fp32 (no quantization needed).
#ifndef MELLO_IOS_NO_VAD
#include "ort_loader.hpp"
#endif
#include <array>
#include <atomic>
#include <cstdint>
#include <deque>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

namespace mello::audio {

static constexpr int SUP_SR16 = 16000;
static constexpr int SUP_FRAME16 = 320;    // 20 ms @16 kHz
static constexpr int SUP_HIDDEN = 322;
static constexpr int SUP_FEAT = 322;
static constexpr int SUP_BINS = 161;       // rDFT-320 bins
static constexpr int SUP_REF_RING_FRAMES = 32;  // 640 ms of far-end history

class EchoSuppressor {
public:
    EchoSuppressor() = default;
    ~EchoSuppressor();

    EchoSuppressor(const EchoSuppressor&) = delete;
    EchoSuppressor& operator=(const EchoSuppressor&) = delete;

    /// Soft-fail init: false leaves process() a passthrough.
    bool initialize(const std::string& model_path);
    void shutdown();

    void set_enabled(bool enabled);
    bool enabled() const { return enabled_.load(std::memory_order_relaxed); }
    bool ready() const { return initialized_; }

    /// Clear GRU states, reference ring, and restart delay search.
    void reset();

    /// Capture thread: suppress in place (960 int16 @48 kHz). No-op unless
    /// enabled and initialized. Never throws.
    void process(int16_t* frame48k) noexcept;

    /// Playback thread: accumulate 48 kHz playout for the reference ring.
    void feed_far_end(const int16_t* samples48k, size_t count);

    /// Last inference wall time in ms (diagnostics).
    double last_inference_ms() const { return last_inference_ms_.load(); }

private:
    void run_inference(const float* mic16, const float* far16, float* out16);
    // FIR decimate-by-3 / interpolate-x3 with persistent history + phase
    // (arbitrary chunk sizes safe).
    void decimate(const int16_t* in, int count, std::vector<float>& hist, int& phase,
                  std::vector<float>& out);
    void interpolate(const float* in, int16_t* out);
    void dft320(const float* in, double* re, double* im);
    void idft320(const double* re, const double* im, float* out);
    // Reference frame for the current delay offset (null while priming).
    // Capture-thread only.
    const float* ref_frame();

    bool initialized_ = false;
    std::atomic<bool> enabled_{false};

#ifndef MELLO_IOS_NO_VAD
    std::unique_ptr<OrtHandles> ort_;
    Ort::Session* session_ = nullptr;
#endif

    // Front-end tables (built at init). Double precision matches the
    // training-time numpy front-end; float32 drift accumulates audibly
    // through 322 recurrent steps per frame over long streams.
    std::vector<float> hann_sqrt_;          // applied in float32
    std::vector<double> dft_cos_, dft_sin_;
    std::vector<float> lp_taps_;            // decimate/interpolate FIR
    std::vector<float> us_state_;           // interpolate history

    // Recurrent + reference state. GRU states, ring, and search bookkeeping
    // run inline on the capture thread; only far_pending48_ crosses threads
    // (playback feeder) under pending_mutex_, and reset() only requests.
    std::vector<float> gru_h01_, gru_h02_;
    std::mutex pending_mutex_;
    std::deque<std::array<float, SUP_FRAME16>> ref_ring_;
    std::vector<int16_t> far_pending48_;
    std::atomic<bool> reset_requested_{false};
    std::vector<float> ds_state_;
    std::vector<float> far_ds_state_;
    int ds_phase_ = 0;
    int far_ds_phase_ = 0;
    std::atomic<double> last_inference_ms_{0.0};
    int over_budget_warned_ = 0;
};

}  // namespace mello::audio
