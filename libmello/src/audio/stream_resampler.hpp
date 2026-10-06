#pragma once
// Fixed-ratio resampler for a continuous audio stream: WebRTC
// PushSincResampler (common_audio/resampler, in the vendored APM tree), one
// per channel, in blocks of about 10 ms.
//
// The WASAPI capture and playback edges use it to convert between the device
// rate and the internal 48 kHz contract (spec 10 section 6.1). It replaces
// linear interpolation, which aliased and dulled the top octave.
//
// This header has no WebRTC include: the implementation compiles in the
// C++20 object library mello_playout (libmello CMakeLists.txt).
//
// Thread: one thread at a time (the device thread that owns it).
// Delay: one input block plus the sinc kernel (about 10.3 ms at 48 kHz).

#include <cstddef>
#include <memory>
#include <vector>

namespace mello::audio {

class StreamResampler {
public:
    /// `channels` interleaved channels at `in_rate` Hz to the same channels
    /// at `out_rate` Hz. Rates must be positive.
    StreamResampler(int in_rate, int out_rate, int channels);
    ~StreamResampler();
    StreamResampler(const StreamResampler&) = delete;
    StreamResampler& operator=(const StreamResampler&) = delete;

    int in_rate() const { return in_rate_; }
    int out_rate() const { return out_rate_; }
    int channels() const { return channels_; }
    /// Input frames that produce one output block.
    size_t in_block_frames() const { return in_block_; }
    /// Output frames per input block.
    size_t out_block_frames() const { return out_block_; }

    /// Append `frames` interleaved input frames. Converts every complete
    /// input block.
    void push(const float* in, size_t frames);
    /// Output frames ready to pull.
    size_t available() const { return out_.size() / static_cast<size_t>(channels_); }
    /// Copy up to `frames` interleaved output frames to `out`. Returns the
    /// frames copied.
    size_t pull(float* out, size_t frames);
    /// Drop all buffered input and output and restart the filters.
    void reset();

private:
    struct Impl;
    int in_rate_;
    int out_rate_;
    int channels_;
    size_t in_block_ = 0;
    size_t out_block_ = 0;
    std::unique_ptr<Impl> impl_;
    std::vector<float> in_;   // interleaved, less than one block
    std::vector<float> out_;  // interleaved, ready to pull
};

} // namespace mello::audio
