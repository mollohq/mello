// iOS clip encode/decode — Step 1 stub.
// AudioToolbox AAC is available on iOS and can be wired up later (see
// IOS-LIBMELLO-PORT §3 "Clip encode"); for the Step 1 link it is inert.
#include "clip_encoder.hpp"
#include "../util/log.hpp"

namespace mello::audio {

bool encode_wav_to_mp4(const std::string& /*wav_path*/,
                       const std::string& /*mp4_path*/,
                       int /*bitrate*/) {
    MELLO_LOG_ERROR("clip_encoder", "iOS stub: MP4/AAC encoding not wired up yet (Step 1)");
    return false;
}

std::vector<int16_t> decode_mp4_to_pcm(const std::string& /*mp4_path*/) {
    MELLO_LOG_ERROR("clip_encoder", "iOS stub: MP4/AAC decoding not wired up yet (Step 1)");
    return {};
}

} // namespace mello::audio
