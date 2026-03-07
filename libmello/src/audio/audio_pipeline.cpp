#include "audio_pipeline.hpp"
#include <cstring>
#include <algorithm>

namespace mello::audio {

AudioPipeline::AudioPipeline() = default;

AudioPipeline::~AudioPipeline() {
    shutdown();
}

bool AudioPipeline::initialize() {
    if (initialized_) return true;

    if (!capture_.initialize()) return false;
    if (!playback_.initialize()) return false;
    if (!encoder_.initialize()) return false;
    if (!noise_suppressor_.initialize()) return false;
    if (!playback_.start()) return false;

    capture_accum_.reserve(FRAME_SIZE * 2);
    initialized_ = true;
    return true;
}

void AudioPipeline::shutdown() {
    stop_capture();
    playback_.stop();
    noise_suppressor_.shutdown();
    initialized_ = false;
}

bool AudioPipeline::start_capture() {
    if (capturing_) return true;
    if (!initialized_) return false;

    bool ok = capture_.start([this](const int16_t* samples, size_t count) {
        on_captured_audio(samples, count);
    });
    if (ok) capturing_ = true;
    return ok;
}

void AudioPipeline::stop_capture() {
    if (!capturing_) return;
    capture_.stop();
    capturing_ = false;

    std::lock_guard<std::mutex> lock(accum_mutex_);
    capture_accum_.clear();
}

void AudioPipeline::set_mute(bool muted) { muted_ = muted; }
void AudioPipeline::set_deafen(bool deafened) { deafened_ = deafened; }

void AudioPipeline::on_captured_audio(const int16_t* samples, size_t count) {
    std::lock_guard<std::mutex> lock(accum_mutex_);

    // Append to accumulation buffer
    capture_accum_.insert(capture_accum_.end(), samples, samples + count);

    // Encode complete frames (FRAME_SIZE samples = 20ms at 48kHz)
    while (capture_accum_.size() >= FRAME_SIZE) {
        if (!muted_) {
            // Apply noise suppression before encoding (also provides VAD)
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
    int size = static_cast<int>(pkt.data.size());
    if (size > buffer_size) {
        outgoing_.pop();
        return 0;
    }
    std::memcpy(buffer, pkt.data.data(), size);
    outgoing_.pop();
    return size;
}

void AudioPipeline::feed_packet(const char* peer_id, const uint8_t* data, int size) {
    if (deafened_ || !initialized_) return;

    std::string pid(peer_id);

    // Initialize decoder if needed
    if (decoders_.find(pid) == decoders_.end()) {
        auto& dec = decoders_[pid];
        if (!dec.initialize()) {
            decoders_.erase(pid);
            return;
        }
    }

    // Push into jitter buffer
    auto& jb = jitter_buffers_[pid];
    uint32_t seq = 0;
    if (size >= 4) {
        // First 4 bytes are sequence number (prepended by sender)
        seq = static_cast<uint32_t>(data[0]) |
              (static_cast<uint32_t>(data[1]) << 8) |
              (static_cast<uint32_t>(data[2]) << 16) |
              (static_cast<uint32_t>(data[3]) << 24);
        jb.push(seq, data + 4, size - 4);
    } else {
        // No sequence header, feed directly
        auto& dec = decoders_[pid];
        int16_t pcm[FRAME_SIZE];
        int samples = dec.decode(data, size, pcm, FRAME_SIZE);
        if (samples > 0) playback_.feed(pcm, static_cast<size_t>(samples));
        return;
    }

    // Pop and decode from jitter buffer
    std::vector<uint8_t> pkt_data;
    while (jb.pop(pkt_data)) {
        auto& dec = decoders_[pid];
        int16_t pcm[FRAME_SIZE];
        int samples = dec.decode(pkt_data.data(), static_cast<int>(pkt_data.size()),
                                 pcm, FRAME_SIZE);
        if (samples > 0) {
            playback_.feed(pcm, static_cast<size_t>(samples));
        }
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

} // namespace mello::audio
