#include "replay_audio_mix.hpp"
#include <algorithm>
#include <cmath>

namespace mello::replay {

namespace {

constexpr double kNominalUsPerFrame = 1'000'000.0 / kClipAudioRate;

uint64_t frames_of(const TimedPcm& chunk, uint32_t channels) {
    return channels > 0 ? chunk.samples.size() / channels : 0;
}

// The gain of a source at time `t`. `cursor` moves forward with `t`.
float gain_at(const ClipAudioSource& source, uint64_t t, size_t& cursor) {
    const auto& points = source.gain_points;
    if (points.empty()) return source.gain;
    while (cursor + 1 < points.size() && points[cursor + 1].first <= t) ++cursor;
    return source.gain * points[cursor].second;
}

} // namespace

namespace detail {

std::vector<AudioRun> fit_runs(const std::vector<TimedPcm>& chunks, uint32_t channels) {
    std::vector<AudioRun> runs;
    if (channels == 0) return runs;

    // Cut at gaps: a chunk that does not start where the chunk before it
    // ends, give or take the jitter of the stamps.
    for (size_t i = 0; i < chunks.size(); ++i) {
        const uint64_t frames = frames_of(chunks[i], channels);
        if (frames == 0) continue;
        bool continues = false;
        if (!runs.empty()) {
            const AudioRun& run = runs.back();
            const TimedPcm& prev = chunks[run.first_chunk + run.chunk_count - 1];
            const double expected =
                static_cast<double>(prev.capture_us) +
                static_cast<double>(frames_of(prev, channels)) * kNominalUsPerFrame;
            continues = run.first_chunk + run.chunk_count == i &&
                        std::abs(static_cast<double>(chunks[i].capture_us) - expected) <= kRunGapUs;
        }
        if (continues) {
            runs.back().chunk_count += 1;
            runs.back().frames += frames;
        } else {
            AudioRun run;
            run.first_chunk = i;
            run.chunk_count = 1;
            run.frames = frames;
            runs.push_back(run);
        }
    }

    // A least-squares line through (frame index, time) of each chunk start.
    for (AudioRun& run : runs) {
        const double t_first = static_cast<double>(chunks[run.first_chunk].capture_us);
        std::vector<double> n_at;
        std::vector<double> t_at;
        double n = 0;
        for (size_t k = 0; k < run.chunk_count; ++k) {
            const TimedPcm& c = chunks[run.first_chunk + k];
            if (c.samples.empty()) continue;
            n_at.push_back(n);
            t_at.push_back(static_cast<double>(c.capture_us) - t_first);
            n += static_cast<double>(frames_of(c, channels));
        }
        const double count = static_cast<double>(n_at.size());
        double mean_n = 0, mean_t = 0;
        for (size_t k = 0; k < n_at.size(); ++k) {
            mean_n += n_at[k];
            mean_t += t_at[k];
        }
        mean_n /= count;
        mean_t /= count;
        double cov = 0, var = 0;
        for (size_t k = 0; k < n_at.size(); ++k) {
            cov += (n_at[k] - mean_n) * (t_at[k] - mean_t);
            var += (n_at[k] - mean_n) * (n_at[k] - mean_n);
        }
        double slope = var > 0 ? cov / var : kNominalUsPerFrame;
        const double ppm = std::abs(slope / kNominalUsPerFrame - 1.0) * 1e6;
        if (n_at.size() < 2 || ppm > kMaxDriftPpm) slope = kNominalUsPerFrame;
        run.us_per_frame = slope;
        run.start_us = t_first + (mean_t - slope * mean_n);
    }
    return runs;
}

void limit(std::vector<float>& stereo) {
    const size_t frames = stereo.size() / 2;
    if (frames == 0) return;
    const float ceiling = std::pow(10.0f, -1.0f / 20.0f);
    const float lookahead = static_cast<float>(kClipAudioRate) * 0.005f;
    const float release = 1.0f - std::exp(-1.0f / (static_cast<float>(kClipAudioRate) * 0.1f));

    // The gain each frame needs, then a ramp down over the lookahead before
    // each peak, so the gain is already low when the peak comes.
    std::vector<float> target(frames);
    for (size_t i = 0; i < frames; ++i) {
        const float peak = std::max(std::abs(stereo[2 * i]), std::abs(stereo[2 * i + 1]));
        target[i] = peak > ceiling ? ceiling / peak : 1.0f;
    }
    for (size_t i = frames - 1; i-- > 0;) {
        target[i] = std::min(target[i], target[i + 1] + 1.0f / lookahead);
    }
    float g = 1.0f;
    for (size_t i = 0; i < frames; ++i) {
        g = std::min(g + (1.0f - g) * release, target[i]);
        stereo[2 * i] *= g;
        stereo[2 * i + 1] *= g;
    }
}

} // namespace detail

std::vector<int16_t> mix_clip_audio(uint64_t t0_us, uint64_t end_us,
                                    const std::vector<ClipAudioSource>& sources) {
    if (end_us <= t0_us) return {};
    const size_t out_frames =
        static_cast<size_t>((end_us - t0_us) * static_cast<uint64_t>(kClipAudioRate) / 1'000'000);
    std::vector<float> mix(out_frames * 2, 0.0f);
    const double t0 = static_cast<double>(t0_us);

    for (const ClipAudioSource& source : sources) {
        const uint32_t ch = source.channels;
        if (ch != 1 && ch != 2) continue;
        const auto runs = detail::fit_runs(source.chunks, ch);
        for (const auto& run : runs) {
            // The run as one block of float samples.
            std::vector<float> s;
            s.reserve(static_cast<size_t>(run.frames) * ch);
            for (size_t k = 0; k < run.chunk_count; ++k) {
                for (const int16_t v : source.chunks[run.first_chunk + k].samples) {
                    s.push_back(static_cast<float>(v) / 32768.0f);
                }
            }
            const double last = static_cast<double>(run.frames - 1);
            const double run_end = run.start_us + last * run.us_per_frame;
            // The output frames whose time falls inside the run.
            const double m_first = std::ceil((run.start_us - t0) / kNominalUsPerFrame);
            const double m_last = std::floor((run_end - t0) / kNominalUsPerFrame);
            const size_t m_begin = static_cast<size_t>(std::max(0.0, m_first));
            const double m_stop = std::min(m_last + 1.0, static_cast<double>(out_frames));
            if (m_stop <= static_cast<double>(m_begin)) continue;
            const size_t m_end = static_cast<size_t>(m_stop);

            size_t cursor = 0;
            for (size_t m = m_begin; m < m_end; ++m) {
                const double t = t0 + static_cast<double>(m) * kNominalUsPerFrame;
                double n = (t - run.start_us) / run.us_per_frame;
                n = std::clamp(n, 0.0, last);
                const size_t i = static_cast<size_t>(n);
                const size_t j = std::min<size_t>(i + 1, static_cast<size_t>(run.frames - 1));
                const float f = static_cast<float>(n - static_cast<double>(i));
                const float g = gain_at(source, static_cast<uint64_t>(t), cursor);
                if (ch == 1) {
                    const float v = (s[i] * (1.0f - f) + s[j] * f) * g;
                    mix[2 * m] += v;
                    mix[2 * m + 1] += v;
                } else {
                    mix[2 * m] += (s[2 * i] * (1.0f - f) + s[2 * j] * f) * g;
                    mix[2 * m + 1] += (s[2 * i + 1] * (1.0f - f) + s[2 * j + 1] * f) * g;
                }
            }
        }
    }

    detail::limit(mix);
    std::vector<int16_t> out(mix.size());
    for (size_t i = 0; i < mix.size(); ++i) {
        const float v = std::clamp(mix[i] * 32768.0f, -32768.0f, 32767.0f);
        out[i] = static_cast<int16_t>(std::lrint(v));
    }
    return out;
}

} // namespace mello::replay
