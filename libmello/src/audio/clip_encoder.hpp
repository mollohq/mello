#pragma once
#include <string>
#include <vector>
#include <cstdint>

namespace mello::audio {

// Encode a WAV file to MP4/AAC-LC using platform media APIs.
// Standalone — no MelloContext or AudioPipeline needed.
// Returns true on success.
bool encode_wav_to_mp4(const std::string& wav_path,
                       const std::string& mp4_path,
                       int bitrate = 64000);

// Decode an MP4/AAC file to mono 48kHz 16-bit PCM.
// Returns decoded samples, empty on failure.
std::vector<int16_t> decode_mp4_to_pcm(const std::string& mp4_path);

namespace detail {

// Shared WAV reader used by platform encoder implementations.
bool read_wav_pcm(const std::string& path,
                  std::vector<int16_t>& pcm,
                  uint32_t& sample_rate,
                  uint16_t& channels);

} // namespace detail

} // namespace mello::audio
