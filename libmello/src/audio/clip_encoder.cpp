#include "clip_encoder.hpp"
#include "../util/log.hpp"
#include <fstream>

namespace mello::audio::detail {

bool read_wav_pcm(const std::string& path,
                  std::vector<int16_t>& pcm,
                  uint32_t& sample_rate,
                  uint16_t& channels) {
    std::ifstream f(path, std::ios::binary);
    if (!f) return false;

    char hdr[44];
    f.read(hdr, 44);
    if (!f || std::string(hdr, 4) != "RIFF" || std::string(hdr + 8, 4) != "WAVE") {
        return false;
    }

    channels = *reinterpret_cast<uint16_t*>(hdr + 22);
    sample_rate = *reinterpret_cast<uint32_t*>(hdr + 24);
    uint16_t bps = *reinterpret_cast<uint16_t*>(hdr + 34);
    uint32_t data_size = *reinterpret_cast<uint32_t*>(hdr + 40);

    if (bps != 16) {
        MELLO_LOG_ERROR("clip_encoder", "WAV must be 16-bit PCM (got %d-bit)", bps);
        return false;
    }

    pcm.resize(data_size / sizeof(int16_t));
    f.read(reinterpret_cast<char*>(pcm.data()), data_size);
    return true;
}

} // namespace mello::audio::detail

// Stub implementations for platforms without a dedicated encoder file.
// Windows and macOS override these via clip_encoder_wmf.cpp / clip_encoder_audiotoolbox.cpp.
#if !defined(_WIN32) && !defined(__APPLE__)

namespace mello::audio {

bool encode_wav_to_mp4(const std::string& /*wav_path*/,
                       const std::string& /*mp4_path*/,
                       int /*bitrate*/) {
    MELLO_LOG_ERROR("clip_encoder", "MP4/AAC encoding not available on this platform");
    return false;
}

std::vector<int16_t> decode_mp4_to_pcm(const std::string& /*mp4_path*/) {
    MELLO_LOG_ERROR("clip_encoder", "MP4/AAC decoding not available on this platform");
    return {};
}

} // namespace mello::audio

#endif
