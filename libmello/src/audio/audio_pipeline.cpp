#include "audio_pipeline.hpp"
#include "../util/log.hpp"
#include <cstring>
#include <algorithm>
#include <cmath>

namespace mello::audio {

AudioPipeline::AudioPipeline() = default;

AudioPipeline::~AudioPipeline() {
    shutdown();
}

bool AudioPipeline::initialize() {
    if (initialized_) return true;
    MELLO_LOG_INFO("pipeline", "initializing audio pipeline");

    device_enum_ = create_device_enumerator();

    capture_ = std::make_unique<WasapiCapture>();
    if (!capture_->initialize()) {
        MELLO_LOG_ERROR("pipeline", "capture init failed");
        return false;
    }

    playback_ = std::make_unique<WasapiPlayback>();
    if (!playback_->initialize()) {
        MELLO_LOG_ERROR("pipeline", "playback init failed");
        return false;
    }

    if (!encoder_.initialize()) {
        MELLO_LOG_ERROR("pipeline", "opus encoder init failed");
        return false;
    }
    if (!noise_suppressor_.initialize()) {
        MELLO_LOG_ERROR("pipeline", "noise suppressor init failed");
        return false;
    }
    if (!playback_->start()) {
        MELLO_LOG_ERROR("pipeline", "playback start failed");
        return false;
    }

    capture_accum_.reserve(FRAME_SIZE * 2);
    initialized_ = true;
    MELLO_LOG_INFO("pipeline", "audio pipeline ready (frame=%d samples, %dHz mono)",
                   FRAME_SIZE, SAMPLE_RATE);
    return true;
}

void AudioPipeline::shutdown() {
    MELLO_LOG_INFO("pipeline", "shutting down");
    stop_capture();
    if (playback_) playback_->stop();
    noise_suppressor_.shutdown();
    capture_.reset();
    playback_.reset();
    initialized_ = false;
}

bool AudioPipeline::start_capture() {
    if (capturing_) return true;
    if (!initialized_ || !capture_) return false;

    bool ok = capture_->start([this](const int16_t* samples, size_t count) {
        on_captured_audio(samples, count);
    });
    if (ok) capturing_ = true;
    return ok;
}

void AudioPipeline::stop_capture() {
    if (!capturing_) return;
    if (capture_) capture_->stop();
    capturing_ = false;

    std::lock_guard<std::mutex> lock(accum_mutex_);
    capture_accum_.clear();
}

void AudioPipeline::set_mute(bool muted) { muted_ = muted; }
void AudioPipeline::set_deafen(bool deafened) { deafened_ = deafened; }

void AudioPipeline::on_captured_audio(const int16_t* samples, size_t count) {
    std::lock_guard<std::mutex> lock(accum_mutex_);

    capture_accum_.insert(capture_accum_.end(), samples, samples + count);

    while (capture_accum_.size() >= FRAME_SIZE) {
        // Compute RMS of the frame for the VU meter
        {
            double sum = 0.0;
            for (int i = 0; i < FRAME_SIZE; ++i) {
                double s = capture_accum_[i] / 32768.0;
                sum += s * s;
            }
            float rms = static_cast<float>(std::sqrt(sum / FRAME_SIZE));
            float db = 20.0f * std::log10f(rms + 1e-10f);
            float level = (db + 60.0f) / 60.0f;
            if (level < 0.0f) level = 0.0f;
            if (level > 1.0f) level = 1.0f;
            input_level_.store(level, std::memory_order_relaxed);

            static int rms_log_counter = 0;
            if ((++rms_log_counter % 250) == 0) {
                MELLO_LOG_DEBUG("pipeline", "rms=%.6f db=%.1f level=%.3f stored=%.3f",
                                rms, db, level, input_level_.load());
            }
        }

        if (!muted_) {
            noise_suppressor_.process(capture_accum_.data(), FRAME_SIZE);
            update_vad(noise_suppressor_.speech_probability());

            uint8_t packet[MAX_PACKET_SIZE];
            int encoded = encoder_.encode(capture_accum_.data(), FRAME_SIZE,
                                          packet, MAX_PACKET_SIZE);
            if (encoded > 0) {
                std::lock_guard<std::mutex> olock(outgoing_mutex_);
                EncodedPacket pkt;
                pkt.data.assign(packet, packet + encoded);
                pkt.sequence = sequence_++;
                outgoing_.push(std::move(pkt));

                if ((pkt.sequence % 250) == 0) {
                    MELLO_LOG_DEBUG("pipeline", "encode: seq=%u size=%d bytes, vad=%.2f, queue=%zu",
                                    pkt.sequence, encoded, speech_prob_, outgoing_.size());
                }
            } else if (encoded < 0) {
                MELLO_LOG_WARN("pipeline", "opus encode error: %d", encoded);
            }
        }
        capture_accum_.erase(capture_accum_.begin(),
                             capture_accum_.begin() + FRAME_SIZE);
    }
}

int AudioPipeline::get_packet(uint8_t* buffer, int buffer_size) {
    std::lock_guard<std::mutex> lock(outgoing_mutex_);
    if (outgoing_.empty()) return 0;

    auto& pkt = outgoing_.front();
    int payload_size = static_cast<int>(pkt.data.size());
    int total_size = payload_size + 4; // 4-byte sequence header + opus payload
    if (total_size > buffer_size) {
        outgoing_.pop();
        return 0;
    }
    // Prepend little-endian sequence number (matches feed_packet's expectation)
    buffer[0] = static_cast<uint8_t>(pkt.sequence);
    buffer[1] = static_cast<uint8_t>(pkt.sequence >> 8);
    buffer[2] = static_cast<uint8_t>(pkt.sequence >> 16);
    buffer[3] = static_cast<uint8_t>(pkt.sequence >> 24);
    std::memcpy(buffer + 4, pkt.data.data(), payload_size);
    outgoing_.pop();
    return total_size;
}

void AudioPipeline::feed_packet(const char* peer_id, const uint8_t* data, int size) {
    if (deafened_ || !initialized_ || !playback_) {
        MELLO_LOG_DEBUG("pipeline", "feed_packet(%s) dropped: deaf=%d init=%d pb=%d",
                        peer_id, (int)deafened_.load(), initialized_, playback_ ? 1 : 0);
        return;
    }

    std::string pid(peer_id);

    if (decoders_.find(pid) == decoders_.end()) {
        MELLO_LOG_INFO("pipeline", "creating decoder for peer '%s'", peer_id);
        auto& dec = decoders_[pid];
        if (!dec.initialize()) {
            MELLO_LOG_ERROR("pipeline", "opus decoder init failed for '%s'", peer_id);
            decoders_.erase(pid);
            return;
        }
    }

    auto& jb = jitter_buffers_[pid];
    uint32_t seq = 0;
    if (size >= 4) {
        seq = static_cast<uint32_t>(data[0]) |
              (static_cast<uint32_t>(data[1]) << 8) |
              (static_cast<uint32_t>(data[2]) << 16) |
              (static_cast<uint32_t>(data[3]) << 24);
        jb.push(seq, data + 4, size - 4);
    } else {
        MELLO_LOG_WARN("pipeline", "feed_packet(%s): short packet (%d bytes), decoding directly", peer_id, size);
        auto& dec = decoders_[pid];
        int16_t pcm[FRAME_SIZE];
        int samples = dec.decode(data, size, pcm, FRAME_SIZE);
        if (samples > 0) playback_->feed(pcm, static_cast<size_t>(samples));
        return;
    }

    int decoded_count = 0;
    std::vector<uint8_t> pkt_data;
    while (jb.pop(pkt_data)) {
        auto& dec = decoders_[pid];
        int16_t pcm[FRAME_SIZE];
        int samples = dec.decode(pkt_data.data(), static_cast<int>(pkt_data.size()),
                                 pcm, FRAME_SIZE);
        if (samples > 0) {
            playback_->feed(pcm, static_cast<size_t>(samples));
            decoded_count++;
        } else {
            MELLO_LOG_WARN("pipeline", "opus decode error for '%s': %d (pkt_size=%zu)",
                           peer_id, samples, pkt_data.size());
        }
    }

    if ((seq % 250) == 0 && decoded_count > 0) {
        MELLO_LOG_DEBUG("pipeline", "feed(%s): seq=%u decoded=%d jitter_buf=%d",
                        peer_id, seq, decoded_count, jb.buffered_count());
    }
}

void AudioPipeline::update_vad(float prob) {
    static constexpr float VAD_THRESHOLD = 0.5f;
    static constexpr int HOLDOVER_FRAMES = 8;

    speech_prob_ = prob;
    bool now_speaking = (prob >= VAD_THRESHOLD);

    if (now_speaking) {
        vad_holdover_ = HOLDOVER_FRAMES;
    } else if (vad_holdover_ > 0) {
        vad_holdover_--;
        now_speaking = true;
    }

    if (now_speaking != was_speaking_) {
        speaking_ = now_speaking;
        was_speaking_ = now_speaking;
        if (vad_callback_) {
            vad_callback_(now_speaking);
        }
    }
}

AudioDeviceEnumerator& AudioPipeline::device_enumerator() {
    if (!device_enum_) {
        device_enum_ = create_device_enumerator();
    }
    return *device_enum_;
}

bool AudioPipeline::set_capture_device(const char* device_id) {
    MELLO_LOG_INFO("pipeline", "switching capture device (was_capturing=%d)", (int)capturing_.load());

    bool was_capturing = capturing_;
    if (was_capturing && capture_) {
        capture_->stop();
        capturing_ = false;
    }

    capture_ = std::make_unique<WasapiCapture>();
    if (!capture_->initialize(device_id)) {
        MELLO_LOG_ERROR("pipeline", "capture device switch failed");
        return false;
    }

    if (was_capturing) {
        bool ok = capture_->start([this](const int16_t* samples, size_t count) {
            on_captured_audio(samples, count);
        });
        if (ok) capturing_ = true;
        MELLO_LOG_INFO("pipeline", "capture restarted on new device: %s", ok ? "ok" : "FAILED");
        return ok;
    }
    return true;
}

bool AudioPipeline::set_playback_device(const char* device_id) {
    MELLO_LOG_INFO("pipeline", "switching playback device");

    if (playback_) playback_->stop();

    playback_ = std::make_unique<WasapiPlayback>();
    if (!playback_->initialize(device_id)) {
        MELLO_LOG_ERROR("pipeline", "playback device switch failed");
        return false;
    }
    bool ok = playback_->start();
    MELLO_LOG_INFO("pipeline", "playback restarted on new device: %s", ok ? "ok" : "FAILED");
    return ok;
}

} // namespace mello::audio
