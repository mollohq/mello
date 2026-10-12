// See stream_resampler.hpp. Compiled in the C++20 object library
// mello_playout with WIN32_LEAN_AND_MEAN.

#include "stream_resampler.hpp"

#include <algorithm>
#include <cstring>
#include <numeric>

#include "common_audio/resampler/push_sinc_resampler.h"

namespace mello::audio {

struct StreamResampler::Impl {
    std::vector<std::unique_ptr<webrtc::PushSincResampler>> resamplers;
    std::vector<float> src;  // one channel of one input block
    std::vector<float> dst;  // one channel of one output block
};

StreamResampler::StreamResampler(int in_rate, int out_rate, int channels)
    : in_rate_(std::max(1, in_rate)),
      out_rate_(std::max(1, out_rate)),
      channels_(std::max(1, channels)),
      impl_(std::make_unique<Impl>()) {
    // Blocks of 10 ms when both rates allow it (every common device rate).
    // Otherwise the smallest whole-sample block pair of at least 10 ms.
    if (in_rate_ % 100 == 0 && out_rate_ % 100 == 0) {
        in_block_ = static_cast<size_t>(in_rate_ / 100);
        out_block_ = static_cast<size_t>(out_rate_ / 100);
    } else {
        const int g = std::gcd(in_rate_, out_rate_);
        const size_t unit_in = static_cast<size_t>(in_rate_ / g);
        const size_t unit_out = static_cast<size_t>(out_rate_ / g);
        const size_t want = static_cast<size_t>(std::max(1, in_rate_ / 100));
        const size_t k = (want + unit_in - 1) / unit_in;
        in_block_ = unit_in * k;
        out_block_ = unit_out * k;
    }
    reset();
}

StreamResampler::~StreamResampler() = default;

void StreamResampler::reset() {
    impl_->resamplers.clear();
    for (int c = 0; c < channels_; ++c) {
        impl_->resamplers.push_back(
            std::make_unique<webrtc::PushSincResampler>(in_block_, out_block_));
    }
    impl_->src.assign(in_block_, 0.0f);
    impl_->dst.assign(out_block_, 0.0f);
    in_.clear();
    out_.clear();
}

void StreamResampler::push(const float* in, size_t frames) {
    if (!in || frames == 0) return;
    const size_t ch = static_cast<size_t>(channels_);
    in_.insert(in_.end(), in, in + frames * ch);
    size_t consumed = 0;
    while (in_.size() - consumed >= in_block_ * ch) {
        const float* block = in_.data() + consumed;
        const size_t base = out_.size();
        out_.resize(base + out_block_ * ch);
        for (size_t c = 0; c < ch; ++c) {
            for (size_t i = 0; i < in_block_; ++i) impl_->src[i] = block[i * ch + c];
            impl_->resamplers[c]->Resample(impl_->src.data(), in_block_, impl_->dst.data(),
                                           out_block_);
            for (size_t i = 0; i < out_block_; ++i) out_[base + i * ch + c] = impl_->dst[i];
        }
        consumed += in_block_ * ch;
    }
    if (consumed > 0) in_.erase(in_.begin(), in_.begin() + static_cast<std::ptrdiff_t>(consumed));
}

size_t StreamResampler::pull(float* out, size_t frames) {
    const size_t ch = static_cast<size_t>(channels_);
    const size_t n = std::min(frames, available());
    if (n == 0 || !out) return 0;
    std::memcpy(out, out_.data(), n * ch * sizeof(float));
    out_.erase(out_.begin(), out_.begin() + static_cast<std::ptrdiff_t>(n * ch));
    return n;
}

} // namespace mello::audio
