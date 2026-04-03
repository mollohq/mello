#include "audio_pipeline.hpp"
#include "../util/log.hpp"
#include <cstring>
#include <algorithm>
#include <cmath>
#include <fstream>

#ifdef _WIN32
#include <windows.h>
#include "playback_wasapi.hpp"
#elif defined(__APPLE__)
#include <mach-o/dyld.h>
#include <libgen.h>
#include <climits>
#else
#include <unistd.h>
#include <limits.h>
#include <libgen.h>
#endif

namespace mello::audio {

static std::string get_exe_dir() {
#ifdef _WIN32
    char buf[MAX_PATH];
    DWORD len = GetModuleFileNameA(nullptr, buf, MAX_PATH);
    if (len == 0) return ".";
    std::string path(buf, len);
    auto pos = path.find_last_of("\\/");
    return (pos != std::string::npos) ? path.substr(0, pos) : ".";
#elif defined(__APPLE__)
    char buf[PATH_MAX];
    uint32_t size = sizeof(buf);
    if (_NSGetExecutablePath(buf, &size) != 0) return ".";
    // Resolve symlinks
    char resolved[PATH_MAX];
    if (!realpath(buf, resolved)) return ".";
    return std::string(dirname(resolved));
#else
    char buf[PATH_MAX];
    ssize_t len = readlink("/proc/self/exe", buf, sizeof(buf) - 1);
    if (len <= 0) return ".";
    buf[len] = '\0';
    return std::string(dirname(buf));
#endif
}

static std::string find_model_path() {
    std::string exe_dir = get_exe_dir();

    // Check next to executable first
    std::string p1 = exe_dir + "/silero_vad.onnx";
    if (std::ifstream(p1).good()) return p1;

    // Check models/ subdirectory next to exe
    std::string p2 = exe_dir + "/models/silero_vad.onnx";
    if (std::ifstream(p2).good()) return p2;

    // Check source tree path (development)
    std::string p3 = exe_dir + "/../libmello/models/silero_vad.onnx";
    if (std::ifstream(p3).good()) return p3;

    // Walk up from exe looking for libmello/models (handles target/debug layout)
    std::string dir = exe_dir;
    for (int i = 0; i < 5; ++i) {
        std::string candidate = dir + "/libmello/models/silero_vad.onnx";
        if (std::ifstream(candidate).good()) return candidate;
        auto pos = dir.find_last_of("\\/");
        if (pos == std::string::npos) break;
        dir = dir.substr(0, pos);
    }

    MELLO_LOG_WARN("pipeline", "silero_vad.onnx not found, searched from: %s", exe_dir.c_str());
    return "";
}

AudioPipeline::AudioPipeline() = default;

AudioPipeline::~AudioPipeline() {
    shutdown();
}

bool AudioPipeline::initialize() {
    if (initialized_) return true;
    MELLO_LOG_INFO("pipeline", "initializing audio pipeline");

    device_enum_ = create_device_enumerator();

#ifdef _WIN32
    session_win_ = std::make_unique<AudioSessionWin>();
    session_win_->initialize();
#endif

    capture_ = create_audio_capture();
    if (!capture_->initialize()) {
        MELLO_LOG_ERROR("pipeline", "capture init failed");
        return false;
    }

    playback_ = create_audio_playback();
#ifdef _WIN32
    apply_session(playback_.get());
#endif
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

    std::string model_path = find_model_path();
    if (model_path.empty() || !vad_.initialize(model_path)) {
        MELLO_LOG_ERROR("pipeline", "Silero VAD init failed (model_path=%s)", model_path.c_str());
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
    vad_.shutdown();
    capture_.reset();
    playback_.reset();
#ifdef _WIN32
    if (session_win_) {
        session_win_->shutdown();
    }
#endif
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
        }

        if (!muted_) {
            vad_.feed(capture_accum_.data(), FRAME_SIZE);
            noise_suppressor_.process(capture_accum_.data(), FRAME_SIZE);

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
                                    pkt.sequence, encoded, vad_.probability(), outgoing_.size());
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
    int total_size = payload_size + 4;
    if (total_size > buffer_size) {
        outgoing_.pop();
        return 0;
    }
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

    capture_ = create_audio_capture();
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

    playback_ = create_audio_playback();
#ifdef _WIN32
    apply_session(playback_.get());
#endif
    if (!playback_->initialize(device_id)) {
        MELLO_LOG_ERROR("pipeline", "playback device switch failed");
        return false;
    }
    bool ok = playback_->start();
    MELLO_LOG_INFO("pipeline", "playback restarted on new device: %s", ok ? "ok" : "FAILED");
    return ok;
}

#ifdef _WIN32
void AudioPipeline::apply_session(AudioPlayback* pb) {
    if (!session_win_ || !pb) return;
    static_cast<WasapiPlayback*>(pb)->set_session(session_win_.get());
}
#endif

} // namespace mello::audio
