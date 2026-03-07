#pragma once
#include <opus.h>
#include <cstdint>
#include <vector>

namespace mello::audio {

// 20ms at 48kHz mono = 960 samples
static constexpr int FRAME_SIZE = 960;
static constexpr int SAMPLE_RATE = 48000;
static constexpr int CHANNELS = 1;
static constexpr int BITRATE = 64000;
static constexpr int MAX_PACKET_SIZE = 4000;

class OpusEnc {
public:
    OpusEnc();
    ~OpusEnc();

    bool initialize(int sample_rate = SAMPLE_RATE, int channels = CHANNELS, int bitrate = BITRATE);

    // Encode a frame of PCM samples. Returns encoded size, or negative on error.
    int encode(const int16_t* pcm, int frame_size, uint8_t* out, int max_out);

    void set_bitrate(int bitrate);

private:
    ::OpusEncoder* encoder_ = nullptr;
};

class OpusDec {
public:
    OpusDec();
    ~OpusDec();

    bool initialize(int sample_rate = SAMPLE_RATE, int channels = CHANNELS);

    // Decode a packet. Returns number of samples per channel, or negative on error.
    int decode(const uint8_t* data, int len, int16_t* pcm, int max_frame_size);

    // Packet loss concealment
    int decode_plc(int16_t* pcm, int frame_size);

private:
    ::OpusDecoder* decoder_ = nullptr;
};

} // namespace mello::audio
