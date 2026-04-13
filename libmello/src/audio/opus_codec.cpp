#include "opus_codec.hpp"

namespace mello::audio {

// --- Encoder ---

OpusEnc::OpusEnc() = default;

OpusEnc::~OpusEnc() {
    if (encoder_) opus_encoder_destroy(encoder_);
}

bool OpusEnc::initialize(int sample_rate, int channels, int bitrate) {
    int err = 0;
    encoder_ = opus_encoder_create(sample_rate, channels, OPUS_APPLICATION_VOIP, &err);
    if (err != OPUS_OK || !encoder_) return false;

    opus_encoder_ctl(encoder_, OPUS_SET_BITRATE(bitrate));
    opus_encoder_ctl(encoder_, OPUS_SET_SIGNAL(OPUS_SIGNAL_VOICE));
    opus_encoder_ctl(encoder_, OPUS_SET_INBAND_FEC(1));
    opus_encoder_ctl(encoder_, OPUS_SET_PACKET_LOSS_PERC(5));
    opus_encoder_ctl(encoder_, OPUS_SET_DTX(1));
    return true;
}

int OpusEnc::encode(const int16_t* pcm, int frame_size, uint8_t* out, int max_out) {
    if (!encoder_) return -1;
    return opus_encode(encoder_, pcm, frame_size, out, max_out);
}

void OpusEnc::set_bitrate(int bitrate) {
    if (encoder_) opus_encoder_ctl(encoder_, OPUS_SET_BITRATE(bitrate));
}

// --- Decoder ---

OpusDec::OpusDec() = default;

OpusDec::~OpusDec() {
    if (decoder_) opus_decoder_destroy(decoder_);
}

bool OpusDec::initialize(int sample_rate, int channels) {
    int err = 0;
    decoder_ = opus_decoder_create(sample_rate, channels, &err);
    return (err == OPUS_OK && decoder_ != nullptr);
}

int OpusDec::decode(const uint8_t* data, int len, int16_t* pcm, int max_frame_size) {
    if (!decoder_) return -1;
    return opus_decode(decoder_, data, len, pcm, max_frame_size, 0);
}

int OpusDec::decode_fec(const uint8_t* data, int len, int16_t* pcm, int max_frame_size) {
    if (!decoder_) return -1;
    return opus_decode(decoder_, data, len, pcm, max_frame_size, 1);
}

int OpusDec::decode_plc(int16_t* pcm, int frame_size) {
    if (!decoder_) return -1;
    return opus_decode(decoder_, nullptr, 0, pcm, frame_size, 0);
}

} // namespace mello::audio
