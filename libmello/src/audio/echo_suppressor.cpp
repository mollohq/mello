#include "echo_suppressor.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstddef>
#include <cstring>

#ifndef MELLO_IOS_NO_VAD
#include <onnxruntime_cxx_api.h>
#endif

namespace mello::audio {

namespace {

// Windowed-sinc lowpass for 3:1 decimation / 1:3 interpolation at 48 kHz.
// Cutoff 7.3 kHz, 48 taps, Kaiser beta 5. Unity DC gain; the interpolator
// compensates zero-stuff loss with an explicit x3 (see upsample).
double bessel_i0(double x) {
    double sum = 1.0, term = 1.0;
    const double xx = x * x / 4.0;
    for (int k = 1; k <= 25; ++k) {
        term *= xx / (k * k);
        sum += term;
    }
    return sum;
}

std::vector<float> make_lp_taps() {
    constexpr int n = 48;
    std::vector<float> taps(n);
    const double fc = 7300.0 / 48000.0;
    const double beta = 5.0;
    const double i0b = bessel_i0(beta);
    double sum = 0.0;
    for (int i = 0; i < n; ++i) {
        const double m = i - (n - 1) / 2.0;
        const double sinc = (m == 0.0) ? 2.0 * fc
                                       : std::sin(2.0 * M_PI * fc * m) / (M_PI * m);
        const double t = 2.0 * i / (n - 1) - 1.0;
        const double w = bessel_i0(beta * std::sqrt(std::max(0.0, 1.0 - t * t))) / i0b;
        taps[i] = static_cast<float>(sinc * w);
        sum += taps[i];
    }
    for (float& t : taps) t = static_cast<float>(t / sum);
    return taps;
}

float rms_f(const float* x, int n) {
    double sum = 0.0;
    for (int i = 0; i < n; ++i) sum += static_cast<double>(x[i]) * x[i];
    return static_cast<float>(std::sqrt(sum / n));
}

}  // namespace

EchoSuppressor::~EchoSuppressor() {
    shutdown();
}

bool EchoSuppressor::initialize(const std::string& model_path) {
#ifdef MELLO_IOS_NO_VAD
    (void)model_path;
    MELLO_LOG_WARN("echo", "suppressor stubbed (MELLO_IOS_NO_VAD) — ORT not linked");
    return false;
#else
    if (model_path.empty()) {
        MELLO_LOG_WARN("echo", "suppressor model not found; stage stays passthrough");
        return false;
    }
    ort_ = init_ort(model_path);
    if (!ort_) return false;
    session_ = open_ort_session(*ort_, model_path, "echo");
    if (!session_) {
        ort_.reset();
        return false;
    }

    // Front-end tables.
    hann_sqrt_.resize(SUP_FRAME16);
    for (int n = 0; n < SUP_FRAME16; ++n) {
        // Periodic Hann (matches the training front-end), square-rooted.
        const double w = 0.54 - 0.46 * std::cos(2.0 * M_PI * n / SUP_FRAME16);
        hann_sqrt_[n] = static_cast<float>(std::sqrt(w));
    }
    dft_cos_.resize(SUP_FRAME16);
    dft_sin_.resize(SUP_FRAME16);
    for (int k = 0; k < SUP_FRAME16; ++k) {
        dft_cos_[k] = std::cos(2.0 * M_PI * k / SUP_FRAME16);
        dft_sin_[k] = std::sin(2.0 * M_PI * k / SUP_FRAME16);
    }
    lp_taps_ = make_lp_taps();
    ds_state_.assign(lp_taps_.size() - 1, 0.0f);
    far_ds_state_.assign(lp_taps_.size() - 1, 0.0f);
    us_state_.assign(lp_taps_.size() - 1, 0.0f);
    ds_phase_ = 0;
    far_ds_phase_ = 0;

    gru_h01_.assign(SUP_HIDDEN, 0.0f);
    gru_h02_.assign(SUP_HIDDEN, 0.0f);
    reset();

    initialized_ = true;
    MELLO_LOG_INFO("echo", "suppressor ready (model=%s)", model_path.c_str());
    return true;
#endif  // MELLO_IOS_NO_VAD
}

void EchoSuppressor::shutdown() {
#ifndef MELLO_IOS_NO_VAD
    if (session_) {
        delete session_;
        session_ = nullptr;
    }
    ort_.reset();
#endif
    initialized_ = false;
}

void EchoSuppressor::set_enabled(bool enabled) {
    enabled_.store(enabled, std::memory_order_relaxed);
    if (enabled) reset();
    MELLO_LOG_INFO("echo", "neural suppression %s%s", enabled ? "enabled" : "disabled",
                   (enabled && !initialized_) ? " (no model: passthrough)" : "");
}

void EchoSuppressor::reset() {
    {
        std::lock_guard<std::mutex> lock(pending_mutex_);
        far_pending48_.clear();
    }
    // State reset runs inline on the capture thread (see process()) to
    // avoid cross-thread vector races; here just request it.
    reset_requested_.store(true, std::memory_order_relaxed);
}

void EchoSuppressor::feed_far_end(const int16_t* samples48k, size_t count) {
    if (!samples48k || count == 0) return;
    if (!enabled_.load(std::memory_order_relaxed)) return;  // no unbounded growth
    std::lock_guard<std::mutex> lock(pending_mutex_);
    far_pending48_.insert(far_pending48_.end(), samples48k, samples48k + count);
    // Bound: drop oldest beyond ~1 s (capture stall while playback runs).
    constexpr size_t kMaxPending = 48000;
    if (far_pending48_.size() > kMaxPending) {
        far_pending48_.erase(far_pending48_.begin(),
                             far_pending48_.begin() + static_cast<ptrdiff_t>(
                                 far_pending48_.size() - kMaxPending));
    }
}

// FIR decimate-by-3 with persistent phase and history (arbitrary chunk sizes).
void EchoSuppressor::decimate(const int16_t* in, int count, std::vector<float>& hist,
                              int& phase, std::vector<float>& out) {
    const int n = static_cast<int>(lp_taps_.size());
    if (static_cast<int>(hist.size()) != n - 1) hist.assign(n - 1, 0.0f);
    for (int i = 0; i < count; ++i) {
        const float s = static_cast<float>(in[i]) / 32768.0f;
        float acc = lp_taps_[0] * s;
        for (int k = 1; k < n; ++k) acc += lp_taps_[k] * hist[k - 1];
        for (int k = n - 2; k > 0; --k) hist[k] = hist[k - 1];
        if (n > 1) hist[0] = s;
        if (phase == 0) out.push_back(acc);
        phase = (phase + 1) % 3;
    }
}

void EchoSuppressor::interpolate(const float* in, int16_t* out) {
    // Zero-stuff x3 through the lowpass; x3 compensates stuffing loss.
    const int n = static_cast<int>(lp_taps_.size());
    if (static_cast<int>(us_state_.size()) != n - 1) us_state_.assign(n - 1, 0.0f);
    std::vector<float>& hist = us_state_;
    for (int i = 0; i < SUP_FRAME16; ++i) {
        for (int p = 0; p < 3; ++p) {
            const float s = (p == 0) ? in[i] * 3.0f : 0.0f;
            float acc = lp_taps_[0] * s;
            for (int k = 1; k < n; ++k) acc += lp_taps_[k] * hist[k - 1];
            for (int k = n - 2; k > 0; --k) hist[k] = hist[k - 1];
            if (n > 1) hist[0] = s;
            int32_t v = static_cast<int32_t>(acc * 32768.0f);
            if (v > 32767) v = 32767;
            if (v < -32768) v = -32768;
            out[i * 3 + p] = static_cast<int16_t>(v);
        }
    }
}

void EchoSuppressor::dft320(const float* in, double* re, double* im) {
    for (int k = 0; k < SUP_FRAME16; ++k) {
        double sr = 0.0, si = 0.0;
        for (int nn = 0; nn < SUP_FRAME16; ++nn) {
            const int tw = (k * nn) % SUP_FRAME16;
            sr += static_cast<double>(in[nn]) * dft_cos_[tw];
            si -= static_cast<double>(in[nn]) * dft_sin_[tw];
        }
        re[k] = sr;
        im[k] = si;
    }
}

void EchoSuppressor::idft320(const double* re, const double* im, float* out) {
    for (int nn = 0; nn < SUP_FRAME16; ++nn) {
        double s = 0.0;
        for (int k = 0; k < SUP_FRAME16; ++k) {
            const int tw = (k * nn) % SUP_FRAME16;
            s += re[k] * dft_cos_[tw] - im[k] * dft_sin_[tw];
        }
        out[nn] = static_cast<float>(s / SUP_FRAME16);
    }
}

const float* EchoSuppressor::ref_frame() {
    // Newest frame. The model aligns far-end context internally via its
    // recurrent states; the caller feeds same-index mic/far pairs.
    if (ref_ring_.empty()) return nullptr;
    return ref_ring_.back().data();
}

void EchoSuppressor::run_inference(const float* mic16, const float* far16, float* out16) {
#ifndef MELLO_IOS_NO_VAD
    // Analysis window first (matches the training front-end; skipping it
    // smears tonal spectra and collapses the mask on voiced speech).
    float wmic[SUP_FRAME16], wfar[SUP_FRAME16];
    for (int i = 0; i < SUP_FRAME16; ++i) {
        wmic[i] = mic16[i] * hann_sqrt_[i];
        wfar[i] = far16[i] * hann_sqrt_[i];
    }
    double re_m[SUP_FRAME16], im_m[SUP_FRAME16], re_f[SUP_FRAME16], im_f[SUP_FRAME16];
    dft320(wmic, re_m, im_m);
    dft320(wfar, re_f, im_f);

    float feat[SUP_FEAT];
    for (int k = 0; k < SUP_BINS; ++k) {
        const double pm = re_m[k] * re_m[k] + im_m[k] * im_m[k];
        const double pf = re_f[k] * re_f[k] + im_f[k] * im_f[k];
        feat[k] = static_cast<float>(std::log10(std::max(pm, 1e-12)) / 20.0);
        feat[SUP_BINS + k] =
            static_cast<float>(std::log10(std::max(pf, 1e-12)) / 20.0);
    }

    auto t0 = std::chrono::steady_clock::now();
    try {
        auto memory_info = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);
        std::vector<int64_t> feat_shape = {1, 1, SUP_FEAT};
        Ort::Value feat_t = Ort::Value::CreateTensor<float>(
            memory_info, feat, SUP_FEAT, feat_shape.data(), feat_shape.size());
        std::vector<int64_t> h_shape = {1, 1, SUP_HIDDEN};
        Ort::Value h01_t = Ort::Value::CreateTensor<float>(
            memory_info, gru_h01_.data(), SUP_HIDDEN, h_shape.data(), h_shape.size());
        Ort::Value h02_t = Ort::Value::CreateTensor<float>(
            memory_info, gru_h02_.data(), SUP_HIDDEN, h_shape.data(), h_shape.size());
        const char* in_names[] = {"input", "h01", "h02"};
        const char* out_names[] = {"output", "hn1", "hn2"};
        std::vector<Ort::Value> ins;
        ins.push_back(std::move(feat_t));
        ins.push_back(std::move(h01_t));
        ins.push_back(std::move(h02_t));
        auto results = session_->Run(Ort::RunOptions{nullptr}, in_names, ins.data(),
                                     ins.size(), out_names, 3);
        const float* mask = results[0].GetTensorData<float>();
        const float* hn1 = results[1].GetTensorData<float>();
        const float* hn2 = results[2].GetTensorData<float>();
        std::copy(hn1, hn1 + SUP_HIDDEN, gru_h01_.begin());
        std::copy(hn2, hn2 + SUP_HIDDEN, gru_h02_.begin());

        double re_e[SUP_FRAME16] = {}, im_e[SUP_FRAME16] = {};
        for (int k = 0; k < SUP_BINS; ++k) {
            const double m = mask[k];
            re_e[k] = m * re_m[k];
            im_e[k] = m * im_m[k];
        }
        float tmp[SUP_FRAME16];
        idft320(re_e, im_e, tmp);
        for (int i = 0; i < SUP_FRAME16; ++i) out16[i] = tmp[i] * hann_sqrt_[i];
    } catch (const Ort::Exception& e) {
        MELLO_LOG_WARN("echo", "suppressor inference failed: %s (bypass)", e.what());
        std::memcpy(out16, mic16, SUP_FRAME16 * sizeof(float));
    }
    const double ms =
        std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0)
            .count();
    last_inference_ms_.store(ms);
    if (ms > 10.0 && (over_budget_warned_++ % 100) == 0) {
        MELLO_LOG_WARN("echo", "suppressor inference %.2f ms over 10 ms budget", ms);
    }
#else
    (void)mic16;
    (void)far16;
    std::memset(out16, 0, SUP_FRAME16 * sizeof(float));
#endif
}

void EchoSuppressor::process(int16_t* frame48k) noexcept {
    try {
        if (!enabled_.load(std::memory_order_relaxed) || !initialized_) return;
#ifndef MELLO_IOS_NO_VAD
        if (!session_ || !frame48k) return;

        if (reset_requested_.exchange(false, std::memory_order_relaxed)) {
            std::fill(gru_h01_.begin(), gru_h01_.end(), 0.0f);
            std::fill(gru_h02_.begin(), gru_h02_.end(), 0.0f);
            ref_ring_.clear();
        }

        // Drain pending far-end into downsampled ring frames (capture thread).
        {
            std::vector<int16_t> far48;
            {
                std::lock_guard<std::mutex> lock(pending_mutex_);
                far48.swap(far_pending48_);
            }
            if (!far48.empty()) {
                std::vector<float> far16;
                far16.reserve(far48.size() / 3 + 1);
                decimate(far48.data(), static_cast<int>(far48.size()), far_ds_state_,
                         far_ds_phase_, far16);
                for (size_t i = 0; i + SUP_FRAME16 <= far16.size(); i += SUP_FRAME16) {
                    std::array<float, SUP_FRAME16> fr{};
                    std::copy_n(far16.data() + i, SUP_FRAME16, fr.data());
                    ref_ring_.push_back(fr);
                    while (ref_ring_.size() > SUP_REF_RING_FRAMES) ref_ring_.pop_front();
                }
            }
        }

        // Mic to 16 kHz (capture frames are always 960 samples).
        std::vector<float> mic_down;
        mic_down.reserve(SUP_FRAME16);
        decimate(frame48k, 960, ds_state_, ds_phase_, mic_down);
        if (static_cast<int>(mic_down.size()) != SUP_FRAME16) return;  // phase slip guard
        float mic16[SUP_FRAME16];
        std::copy_n(mic_down.data(), SUP_FRAME16, mic16);

        // Always the latest reference frame. The model carries far-end
        // context across frames in its recurrent states (the reference
        // implementation feeds same-index mic/far pairs with no delay
        // compensation); frame-level delay search actively harms it —
        // measured hunting on voiced/reverberant far-end.
        float far16[SUP_FRAME16] = {};
        const float* ref = ref_frame();
        if (ref) std::memcpy(far16, ref, sizeof(far16));
        if (rms_f(far16, SUP_FRAME16) < 0.0005f) {
            return;  // passthrough: frame untouched
        }

        float out16[SUP_FRAME16];
        run_inference(mic16, far16, out16);
        interpolate(out16, frame48k);
#endif
    } catch (...) {
        // Never break audio on model failure.
    }
}

}  // namespace mello::audio
