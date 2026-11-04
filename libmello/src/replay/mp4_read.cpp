#include "mp4_read.hpp"
#include "mp4_demux.hpp"
#include "../util/log.hpp"

#include <opus.h>

#include <memory>
#include <string>

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/read";

struct OpusDecoderDeleter {
    void operator()(OpusDecoder* d) const { opus_decoder_destroy(d); }
};

bool open(const std::filesystem::path& mp4, Mp4ReadAt& read, Mp4Movie& movie) {
    uint64_t size = 0;
    read = file_reader(mp4, &size);
    std::string error;
    if (size == 0 || !open_mp4(read, size, movie, &error)) {
        MELLO_LOG_WARN(TAG, "%s does not open: %s", mp4.u8string().c_str(),
                       error.empty() ? "an empty file" : error.c_str());
        return false;
    }
    return true;
}

}  // namespace

bool read_mp4_video(const std::filesystem::path& mp4,
                    const std::function<bool(const Mp4VideoUnit&)>& on_unit, uint32_t* width,
                    uint32_t* height) {
    Mp4ReadAt read;
    Mp4Movie movie;
    if (!open(mp4, read, movie)) return false;
    const Mp4Track* video = movie.video();
    if (!video) {
        MELLO_LOG_WARN(TAG, "%s has no H.264 track", mp4.u8string().c_str());
        return false;
    }
    if (width) *width = video->width;
    if (height) *height = video->height;
    std::vector<uint8_t> raw;
    Mp4VideoUnit unit;
    bool first_key = true;
    for (const Mp4Sample& s : video->samples) {
        if (!read_sample(read, s, raw)) return false;
        std::vector<uint8_t> annex_b;
        if (!avcc_to_annex_b(raw.data(), raw.size(), video->nal_length_size, annex_b)) {
            MELLO_LOG_WARN(TAG, "%s: a sample is not length-prefixed H.264", mp4.u8string().c_str());
            return false;
        }
        unit.annex_b.clear();
        // The track's parameter sets before the first key frame: a decoder
        // that starts there needs them, and a clip keeps them in band too.
        if (s.sync && first_key) {
            static const uint8_t kStart[] = {0, 0, 0, 1};
            for (const auto& p : video->sps) {
                unit.annex_b.insert(unit.annex_b.end(), kStart, kStart + 4);
                unit.annex_b.insert(unit.annex_b.end(), p.begin(), p.end());
            }
            for (const auto& p : video->pps) {
                unit.annex_b.insert(unit.annex_b.end(), kStart, kStart + 4);
                unit.annex_b.insert(unit.annex_b.end(), p.begin(), p.end());
            }
            first_key = false;
        }
        unit.annex_b.insert(unit.annex_b.end(), annex_b.begin(), annex_b.end());
        unit.time_us = video->time_us(s.time);
        unit.keyframe = s.sync;
        if (!on_unit(unit)) return true;
    }
    return true;
}

bool read_mp4_audio(const std::filesystem::path& mp4,
                    const std::function<bool(const int16_t* stereo, size_t frames, int64_t time_us)>& on_pcm) {
    Mp4ReadAt read;
    Mp4Movie movie;
    if (!open(mp4, read, movie)) return false;
    const Mp4Track* audio = movie.audio();
    if (!audio) {
        MELLO_LOG_WARN(TAG, "%s has no Opus track", mp4.u8string().c_str());
        return false;
    }
    int err = 0;
    std::unique_ptr<OpusDecoder, OpusDecoderDeleter> dec(opus_decoder_create(48000, 2, &err));
    if (!dec || err != OPUS_OK) return false;
    // The samples before the edit's media time decode but are not heard (the
    // pre-skip). The track's timescale is 48 kHz for an Opus clip of ours; a
    // track at another scale has its skip moved to 48 kHz.
    const uint32_t scale = audio->timescale ? audio->timescale : 48000;
    int64_t skip = audio->edit_media_time * 48000 / scale;
    int64_t heard = 0;  // frames given to on_pcm
    std::vector<uint8_t> raw;
    std::vector<int16_t> pcm(5760 * 2);  // 120 ms, the longest Opus packet
    for (const Mp4Sample& s : audio->samples) {
        if (!read_sample(read, s, raw)) return false;
        const int frames = opus_decode(dec.get(), raw.data(), static_cast<opus_int32>(raw.size()), pcm.data(), 5760, 0);
        if (frames < 0) {
            MELLO_LOG_WARN(TAG, "%s: opus_decode failed: %d", mp4.u8string().c_str(), frames);
            return false;
        }
        int64_t from = 0;
        if (skip > 0) {
            from = std::min<int64_t>(skip, frames);
            skip -= from;
        }
        const size_t n = static_cast<size_t>(frames - from);
        if (n == 0) continue;
        if (!on_pcm(pcm.data() + from * 2, n, heard * 1'000'000 / 48000)) return true;
        heard += static_cast<int64_t>(n);
    }
    return true;
}

}  // namespace mello::replay
