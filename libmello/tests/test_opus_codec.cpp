#define _USE_MATH_DEFINES
#include <cmath>
#include <gtest/gtest.h>
#include "audio/opus_codec.hpp"
#include <vector>
#include <numeric>

using namespace mello::audio;

class OpusCodecTest : public ::testing::Test {
protected:
    OpusEnc enc;
    OpusDec dec;
    uint8_t packet[MAX_PACKET_SIZE];
    int16_t pcm_out[FRAME_SIZE];

    void SetUp() override {
        ASSERT_TRUE(enc.initialize());
        ASSERT_TRUE(dec.initialize());
    }

    void generate_sine(int16_t* buf, int samples, float freq_hz, float amplitude = 0.8f) {
        for (int i = 0; i < samples; ++i) {
            float t = static_cast<float>(i) / SAMPLE_RATE;
            buf[i] = static_cast<int16_t>(amplitude * 32767.0f * std::sin(2.0f * M_PI * freq_hz * t));
        }
    }

    double rms(const int16_t* buf, int n) {
        double sum = 0;
        for (int i = 0; i < n; ++i) sum += static_cast<double>(buf[i]) * buf[i];
        return std::sqrt(sum / n);
    }
};

TEST_F(OpusCodecTest, EncodeDecodeSineWave) {
    int16_t pcm_in[FRAME_SIZE];
    generate_sine(pcm_in, FRAME_SIZE, 440.0f);

    int enc_size = enc.encode(pcm_in, FRAME_SIZE, packet, MAX_PACKET_SIZE);
    ASSERT_GT(enc_size, 0) << "encode returned no data";

    int dec_samples = dec.decode(packet, enc_size, pcm_out, FRAME_SIZE);
    ASSERT_EQ(dec_samples, FRAME_SIZE);

    double in_rms = rms(pcm_in, FRAME_SIZE);
    double out_rms = rms(pcm_out, FRAME_SIZE);
    EXPECT_GT(out_rms, in_rms * 0.3) << "decoded energy too low vs input";
}

TEST_F(OpusCodecTest, EncodeSilence) {
    int16_t silence[FRAME_SIZE] = {};
    int enc_size = enc.encode(silence, FRAME_SIZE, packet, MAX_PACKET_SIZE);
    ASSERT_GT(enc_size, 0) << "silence should still produce a packet";
    EXPECT_LT(enc_size, 200) << "silence packet should be small";
}

TEST_F(OpusCodecTest, DecodeInvalidData) {
    uint8_t garbage[] = {0xFF, 0xFE, 0x00, 0x42, 0x99};
    int result = dec.decode(garbage, sizeof(garbage), pcm_out, FRAME_SIZE);
    // Opus may return an error or produce some output; it should NOT crash
    (void)result;
}

TEST_F(OpusCodecTest, MultipleFrames) {
    int16_t pcm_in[FRAME_SIZE];
    generate_sine(pcm_in, FRAME_SIZE, 440.0f);

    for (int frame = 0; frame < 10; ++frame) {
        int enc_size = enc.encode(pcm_in, FRAME_SIZE, packet, MAX_PACKET_SIZE);
        ASSERT_GT(enc_size, 0) << "frame " << frame << " encode failed";

        int dec_samples = dec.decode(packet, enc_size, pcm_out, FRAME_SIZE);
        ASSERT_EQ(dec_samples, FRAME_SIZE) << "frame " << frame << " decode size mismatch";
    }
    double out_rms = rms(pcm_out, FRAME_SIZE);
    EXPECT_GT(out_rms, 1000.0) << "final decoded frame should still have energy";
}

TEST_F(OpusCodecTest, SetBitrate) {
    int16_t pcm_in[FRAME_SIZE];
    generate_sine(pcm_in, FRAME_SIZE, 440.0f);

    int size_64k = enc.encode(pcm_in, FRAME_SIZE, packet, MAX_PACKET_SIZE);
    ASSERT_GT(size_64k, 0);

    enc.set_bitrate(16000);
    int size_16k = enc.encode(pcm_in, FRAME_SIZE, packet, MAX_PACKET_SIZE);
    ASSERT_GT(size_16k, 0);
    EXPECT_LT(size_16k, size_64k) << "lower bitrate should produce smaller packets";
}
