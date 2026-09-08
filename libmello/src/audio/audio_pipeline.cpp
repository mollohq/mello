#include "audio_pipeline.hpp"
#include "../util/log.hpp"
#include <cstring>
#include <algorithm>
#include <cmath>
#include <chrono>
#include <fstream>

#ifdef _WIN32
#include <windows.h>
#include "playback_wasapi.hpp"
#elif defined(__APPLE__)
#include <mach-o/dyld.h>
#include <libgen.h>
#include <climits>
#include "vpio_duplex.hpp"
#else
#include <unistd.h>
#include <limits.h>
#include <libgen.h>
#endif

namespace mello::audio {

static constexpr int SPEECH_PRE_ROLL_FRAMES = 5;       // 100ms at 20ms/frame.
static constexpr int CANDIDATE_HANGOVER_FRAMES = 10;   // Keep Silero warm across brief dips.
static constexpr int SPEECH_ENCODE_HANGOVER_FRAMES = 8;
static constexpr float MIN_SPEECH_RMS = 0.002f;        // ~-54 dBFS, before adaptive floor.
static constexpr float NOISE_FLOOR_GATE_MULT = 2.5f;

static WebRtcNsLevel to_webrtc_ns_mode(NsMode mode) {
    switch (mode) {
        case NsMode::WebRtcLow:
            return WebRtcNsLevel::Low;
        case NsMode::WebRtcModerate:
            return WebRtcNsLevel::Moderate;
        case NsMode::WebRtcHigh:
            return WebRtcNsLevel::High;
        case NsMode::WebRtcVeryHigh:
            return WebRtcNsLevel::VeryHigh;
        case NsMode::Off:
        case NsMode::Rnnoise:
        default:
            return WebRtcNsLevel::Off;
    }
}

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

static std::string find_model_file(const char* filename) {
    std::string exe_dir = get_exe_dir();

    // Check next to executable first
    std::string p1 = exe_dir + "/" + filename;
    if (std::ifstream(p1).good()) return p1;

    // Check models/ subdirectory next to exe
    std::string p2 = exe_dir + "/models/" + filename;
    if (std::ifstream(p2).good()) return p2;

    // Check source tree path (development)
    std::string p3 = exe_dir + "/../libmello/models/" + filename;
    if (std::ifstream(p3).good()) return p3;

    // Walk up from exe looking for libmello/models (handles target/debug layout)
    std::string dir = exe_dir;
    for (int i = 0; i < 5; ++i) {
        std::string candidate = dir + "/libmello/models/" + filename;
        if (std::ifstream(candidate).good()) return candidate;
        auto pos = dir.find_last_of("\\/");
        if (pos == std::string::npos) break;
        dir = dir.substr(0, pos);
    }

    MELLO_LOG_WARN("pipeline", "%s not found, searched from: %s", filename, exe_dir.c_str());
    return "";
}

AudioPipeline::AudioPipeline() = default;

AudioPipeline::~AudioPipeline() {
    shutdown();
}

bool AudioPipeline::initialize() {
    if (initialized_) return true;
    MELLO_LOG_INFO("pipeline", "initializing audio pipeline");

#ifdef _WIN32
    HRESULT com_hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    if (FAILED(com_hr) && com_hr != RPC_E_CHANGED_MODE) {
        MELLO_LOG_ERROR("pipeline", "COM init failed hr=0x%08lx", com_hr);
        return false;
    }
#endif

    device_enum_ = create_device_enumerator();

#ifdef _WIN32
    session_win_ = std::make_unique<AudioSessionWin>();
    session_win_->initialize();
#endif

    capture_ = create_audio_capture();
    playback_ = create_audio_playback();
#ifdef _WIN32
    apply_session(playback_.get());
#endif
#ifdef __APPLE__
    // Voice-session scope: the duplex unit lights the mic at the device
    // level as soon as it starts, so it must not exist before a voice
    // session. Startup is always the plain pair (clips preview mic-free);
    // the toggle applies at capture start. Only the desired backend is
    // stored here.
    voice_processing_capture_.store(echo_canceller_.aec_enabled(),
                                    std::memory_order_relaxed);
#endif
    if (!capture_->initialize(current_capture_device_id())) {
        MELLO_LOG_ERROR("pipeline", "capture init failed");
        return false;
    }

#ifdef _WIN32
    apply_session(playback_.get());
#endif
    if (!playback_->initialize(current_playback_device_id())) {
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

    if (!echo_canceller_.initialize(SAMPLE_RATE, CHANNELS)) {
        MELLO_LOG_ERROR("pipeline", "echo canceller init failed");
        return false;
    }
    set_ns_mode(ns_mode());
    set_transient_suppression(transient_suppression_enabled());
    set_high_pass_filter(high_pass_filter_enabled());

    std::string model_path = find_model_file("silero_vad.onnx");
    if (model_path.empty() || !vad_.initialize(model_path)) {
        MELLO_LOG_ERROR("pipeline", "Silero VAD init failed (model_path=%s)", model_path.c_str());
        return false;
    }

    // Neural suppressor is a soft dependency: missing model degrades to
    // passthrough and the flag stays effective for when it appears.
    std::string echo_model = find_model_file("echo_suppressor.onnx");
    if (!echo_suppressor_.initialize(echo_model)) {
        MELLO_LOG_WARN("pipeline", "echo suppressor unavailable (model_path=%s)",
                       echo_model.c_str());
    }
    playback_->set_render_source([this](int16_t* out, size_t count) -> size_t {
        return mix_output(out, count);
    });

    if (!playback_->start()) {
        MELLO_LOG_ERROR("pipeline", "playback start failed");
        return false;
    }

    capture_accum_.reserve(FRAME_SIZE * 2);
    initialized_ = true;
    refresh_stream_delay_hint();
    echo_suppressor_.reset();
    MELLO_LOG_INFO("pipeline", "audio pipeline ready (frame=%d samples, %dHz mono)",
                   FRAME_SIZE, SAMPLE_RATE);
    return true;
}

void AudioPipeline::shutdown() {
    MELLO_LOG_INFO("pipeline", "shutting down");
    initialized_ = false;  // first: no backend switching while dying
    stop_capture();
    if (playback_) playback_->stop();
    echo_canceller_.shutdown();
    echo_suppressor_.shutdown();
    noise_suppressor_.shutdown();
    vad_.shutdown();
    capture_.reset();
    playback_.reset();
#ifdef _WIN32
    if (session_win_) {
        session_win_->shutdown();
    }
#endif
    capture_inject_mode_.store(false, std::memory_order_relaxed);
}

bool AudioPipeline::start_capture() {
    if (capturing_ && !capture_inject_mode_.load(std::memory_order_relaxed)) return true;
    if (!initialized_ || !capture_) return false;

    if (capture_inject_mode_.load(std::memory_order_relaxed)) {
        capturing_ = false;
        capture_inject_mode_.store(false, std::memory_order_relaxed);
    }

    // Fresh GRU states + delay search for the new speech session.
    echo_suppressor_.reset();

#ifdef __APPLE__
    // Activate the duplex pair for the session when desired and not live.
    // Starting it any earlier shows mic usage with no voice session.
    if (voice_processing_capture_.load(std::memory_order_relaxed) &&
        !(capture_ && capture_->provides_echo_cancellation())) {
        switch_audio_backend(true);
        if (!capture_) return false;
    }
#endif

    // Cache the actual backend echo state for the audio thread (it must
    // not dereference capture_: device switches can replace it).
    backend_cancels_echo_.store(capture_ && capture_->provides_echo_cancellation(),
                                std::memory_order_relaxed);

    bool ok = capture_->start([this](const int16_t* samples, size_t count) {
        on_captured_audio(samples, count);
    });
    if (ok) capturing_ = true;
    return ok;
}

void AudioPipeline::stop_capture() {
    if (capturing_) {
        if (!capture_inject_mode_.load(std::memory_order_relaxed) && capture_) {
            capture_->stop();
        }
        capturing_ = false;
        capture_inject_mode_.store(false, std::memory_order_relaxed);
    }

#ifdef __APPLE__
    // Leaving voice releases the mic: drop a live duplex unit back to the
    // plain pair. Desired stays where the toggle put it (the APM flag
    // mirrors it); only the live backend drops. Skipped while dying.
    if (initialized_ && capture_ && capture_->provides_echo_cancellation()) {
        switch_audio_backend(false);
        voice_processing_capture_.store(echo_canceller_.aec_enabled(),
                                        std::memory_order_relaxed);
    }
#endif

    std::lock_guard<std::mutex> lock(accum_mutex_);
    capture_accum_.clear();
    reset_speech_gate_state();

    // Leaving voice should immediately flush remote decode state so playback
    // cannot keep synthesizing PLC/noise from stale peers after disconnect.
    clear_remote_streams();
}

bool AudioPipeline::start_capture_inject() {
    if (!initialized_) return false;

    if (capturing_ && !capture_inject_mode_.load(std::memory_order_relaxed) && capture_) {
        capture_->stop();
    }

    capturing_ = true;
    capture_inject_mode_.store(true, std::memory_order_relaxed);
    // Same backend snapshot as start_capture(): injected frames must take
    // the same APM path the live backend would.
    backend_cancels_echo_.store(capture_ && capture_->provides_echo_cancellation(),
                                std::memory_order_relaxed);
    MELLO_LOG_INFO("pipeline", "capture inject mode started");
    return true;
}

void AudioPipeline::stop_capture_inject() {
    stop_capture();
    MELLO_LOG_INFO("pipeline", "capture inject mode stopped");
}

void AudioPipeline::inject_capture(const int16_t* samples, int count) {
    if (!samples || count <= 0) return;
    if (!capturing_ || !capture_inject_mode_.load(std::memory_order_relaxed)) return;
    on_captured_audio(samples, static_cast<size_t>(count));
}

void AudioPipeline::set_ns_mode(NsMode mode) {
    ns_mode_.store(static_cast<int>(mode), std::memory_order_relaxed);
    if (mode == NsMode::Rnnoise) {
        noise_suppressor_.set_enabled(true);
    } else {
        noise_suppressor_.set_enabled(false);
    }
    echo_canceller_.set_noise_suppression_level(to_webrtc_ns_mode(mode));
    MELLO_LOG_INFO("pipeline", "noise suppression mode set to %d", static_cast<int>(mode));
}

void AudioPipeline::set_transient_suppression(bool enabled) {
    transient_suppression_enabled_.store(enabled, std::memory_order_relaxed);
    echo_canceller_.set_transient_suppression_enabled(enabled);
}

void AudioPipeline::set_high_pass_filter(bool enabled) {
    high_pass_filter_enabled_.store(enabled, std::memory_order_relaxed);
    echo_canceller_.set_high_pass_filter_enabled(enabled);
}

void AudioPipeline::set_echo_cancellation(bool enabled) {
    echo_canceller_.set_aec_enabled(enabled);
#ifdef __APPLE__
    // On macOS the toggle selects the capture backend: on = combined VPIO
    // duplex (OS AEC/AGC, our APM capture pass skipped), off = plain HAL
    // pair + software AEC. Elsewhere it only flips the APM flag. The
    // switch applies immediately only mid-session; outside voice the
    // desired backend is stored for the next capture start, so no unit
    // ever runs (and no mic shows) without a session.
    if (initialized_ && capturing_ &&
        voice_processing_capture_.load(std::memory_order_relaxed) != enabled) {
        switch_audio_backend(enabled);
    } else {
        voice_processing_capture_.store(enabled, std::memory_order_relaxed);
    }
#endif
}

#ifdef __APPLE__
bool AudioPipeline::activate_vpio_pair() {
    auto unit = VpioUnit::create();
    if (!unit->initialize(current_capture_device_id(), current_playback_device_id())) {
        return false;
    }
    capture_ = std::make_unique<VpioCaptureAdapter>(unit);
    playback_ = std::make_unique<VpioPlaybackAdapter>(unit);
    MELLO_LOG_INFO("pipeline", "VPIO duplex pair active");
    return true;
}

void AudioPipeline::activate_plain_pair() {
    capture_ = create_audio_capture();
    playback_ = create_audio_playback();
#ifdef _WIN32
    apply_session(playback_.get());
#endif
}
#endif

void AudioPipeline::switch_audio_backend(bool voice_processing) {
    MELLO_LOG_INFO("pipeline", "switching audio backend (voice_processing=%d, was_capturing=%d)",
                   (int)voice_processing, (int)capturing_.load());
    voice_processing_capture_.store(voice_processing, std::memory_order_relaxed);

    bool was_capturing = capturing_;
    if (was_capturing && capture_) {
        capture_->stop();
        capturing_ = false;
    }
    if (playback_) playback_->stop();

    bool live = false;
#ifdef __APPLE__
    if (voice_processing) live = activate_vpio_pair();
    if (!live) {
        if (voice_processing) {
            MELLO_LOG_WARN("pipeline", "VPIO duplex unavailable, falling back to plain HAL pair");
        }
        activate_plain_pair();
    }
#else
    // Only reached on Apple (call sites are guarded); plain pair it is.
    (void)voice_processing;
    (void)live;
    capture_ = create_audio_capture();
    playback_ = create_audio_playback();
#ifdef _WIN32
    apply_session(playback_.get());
#endif
#endif

    // (Re)wire both halves before start (device-switch invariants): the
    // capture callback and the render source do not transfer across
    // replacement instances.
    if (!capture_->initialize(current_capture_device_id()) ||
        !playback_->initialize(current_playback_device_id())) {
        MELLO_LOG_WARN("pipeline", "backend switch failed on stored devices, retrying defaults");
        capture_device_id_.clear();
        playback_device_id_.clear();
#ifdef __APPLE__
        activate_plain_pair();
#else
        capture_ = create_audio_capture();
        playback_ = create_audio_playback();
#ifdef _WIN32
        apply_session(playback_.get());
#endif
#endif
        if (!capture_->initialize(nullptr) || !playback_->initialize(nullptr)) {
            MELLO_LOG_ERROR("pipeline", "backend switch failed on fallback too");
            return;
        }
    }
    playback_->set_render_source([this](int16_t* out, size_t count) -> size_t {
        return mix_output(out, count);
    });
    if (!playback_->start()) {
        MELLO_LOG_ERROR("pipeline", "playback restart on backend switch failed");
        return;
    }

    if (was_capturing) {
        bool ok = capture_->start([this](const int16_t* samples, size_t count) {
            on_captured_audio(samples, count);
        });
        if (ok) capturing_ = true;
        MELLO_LOG_INFO("pipeline", "capture restarted on new backend: %s",
                       ok ? (capture_->provides_echo_cancellation() ? "VoiceProcessingIO" : "plain")
                          : "FAILED");
    }
    refresh_stream_delay_hint();
    echo_suppressor_.reset();
}

void AudioPipeline::set_mute(bool muted) { muted_ = muted; }
void AudioPipeline::set_deafen(bool deafened) { deafened_ = deafened; }

void AudioPipeline::reset_speech_gate_state() {
    speech_pre_roll_.clear();
    candidate_hangover_frames_ = 0;
    speech_hangover_frames_ = 0;
    speech_gate_active_ = false;
    vad_.force_silence();
}

void AudioPipeline::set_push_to_talk(bool enabled) {
    push_to_talk_mode_.store(enabled, std::memory_order_relaxed);
    if (enabled) {
        std::lock_guard<std::mutex> lock(accum_mutex_);
        reset_speech_gate_state();
    }
    MELLO_LOG_INFO("pipeline", "push_to_talk mode %s", enabled ? "enabled" : "disabled");
}

void AudioPipeline::process_and_encode_frame(int16_t* frame) {
    if (ns_mode() == NsMode::Rnnoise) {
        noise_suppressor_.process(frame, FRAME_SIZE);
    }

    uint8_t raw_pkt[MAX_PACKET_SIZE];
    int encoded = encoder_.encode(frame, FRAME_SIZE, raw_pkt, MAX_PACKET_SIZE);

    if (encoded > 0) {
        std::lock_guard<std::mutex> olock(outgoing_mutex_);
        EncodedPacket pkt;
        pkt.data.assign(raw_pkt, raw_pkt + encoded);
        pkt.sequence = sequence_++;
        outgoing_.push(std::move(pkt));

        if ((pkt.sequence % 250) == 0 || pkt.sequence < 5) {
            MELLO_LOG_DEBUG("pipeline", "encode: seq=%u size=%d bytes, vad=%.2f, queue=%zu",
                            pkt.sequence, encoded, vad_.probability(), outgoing_.size());
        }
    } else if (encoded < 0) {
        MELLO_LOG_WARN("pipeline", "opus encode error: %d", encoded);
    }
}

void AudioPipeline::on_captured_audio(const int16_t* samples, size_t count) {
    std::lock_guard<std::mutex> lock(accum_mutex_);

    // VPIO path: the OS unit already ran AEC/AGC on this audio. Skipping
    // our APM capture pass avoids double processing. RNNoise + VAD below
    // run unchanged on both paths.
    const bool skip_apm_capture =
        backend_cancels_echo_.load(std::memory_order_relaxed);

    capture_accum_.insert(capture_accum_.end(), samples, samples + count);

    while (capture_accum_.size() >= FRAME_SIZE) {
        float rms = 0.0f;
        {
            double sum = 0.0;
            for (int i = 0; i < FRAME_SIZE; ++i) {
                double s = capture_accum_[i] / 32768.0;
                sum += s * s;
            }
            rms = static_cast<float>(std::sqrt(sum / FRAME_SIZE));
            float db = 20.0f * std::log10f(rms + 1e-10f);
            float level = (db + 60.0f) / 60.0f;
            if (level < 0.0f) level = 0.0f;
            if (level > 1.0f) level = 1.0f;
            input_level_.store(level, std::memory_order_relaxed);
        }

        if (!muted_) {
            float ig = input_gain_.load(std::memory_order_relaxed);
            if (ig != 1.0f) {
                for (int i = 0; i < FRAME_SIZE; ++i) {
                    int32_t s = static_cast<int32_t>(capture_accum_[i] * ig);
                    if (s > 32767) s = 32767;
                    if (s < -32768) s = -32768;
                    capture_accum_[i] = static_cast<int16_t>(s);
                }
            }

            if (!skip_apm_capture) {
                echo_canceller_.process_capture(capture_accum_.data(), FRAME_SIZE);
            }

            // NEURAL RESIDUAL-ECHO INSERTION POINT: a two-input
            // (post-AEC mic + far-end reference) suppressor runs here, before
            // the gate/VAD below.
            // Iteration 1 runs on the software path only: the VPIO duplex
            // already applies OS AEC, and stacking an extra stage there is
            // an unmeasured experiment for later.
            bool suppressor_ran = false;
            if (!skip_apm_capture && echo_suppressor_.enabled()) {
                echo_suppressor_.process(capture_accum_.data());
                suppressor_ran = true;
            }

            // The gate threshold uses post-stage RMS once the suppressor
            // runs, so echo residue alone cannot hold the gate open. Pre-AEC
            // behavior is unchanged with the flag off.
            float gate_rms = rms;
            if (suppressor_ran) {
                double sum = 0.0;
                for (int i = 0; i < FRAME_SIZE; ++i) {
                    double s = capture_accum_[i] / 32768.0;
                    sum += s * s;
                }
                gate_rms = static_cast<float>(std::sqrt(sum / FRAME_SIZE));
            }

            if (local_clip_ring_ && clip_buffer_ && clip_buffer_->is_active()) {
                local_clip_ring_->write(capture_accum_.data(), FRAME_SIZE);
            }

            if (push_to_talk_mode_.load(std::memory_order_relaxed)) {
                process_and_encode_frame(capture_accum_.data());
            } else {
                const float speech_threshold =
                    (std::max)(MIN_SPEECH_RMS, noise_floor_rms_ * NOISE_FLOOR_GATE_MULT);
                bool candidate_speech = gate_rms >= speech_threshold;

                if (candidate_speech) {
                    candidate_hangover_frames_ = CANDIDATE_HANGOVER_FRAMES;
                } else if (candidate_hangover_frames_ > 0) {
                    candidate_hangover_frames_--;
                }

                bool should_run_vad = candidate_speech || candidate_hangover_frames_ > 0 ||
                                      speech_hangover_frames_ > 0;

                if (should_run_vad) {
                    vad_.feed(capture_accum_.data(), FRAME_SIZE);
                }

                if (vad_.is_speaking()) {
                    speech_hangover_frames_ = SPEECH_ENCODE_HANGOVER_FRAMES;
                } else if (speech_hangover_frames_ > 0) {
                    speech_hangover_frames_--;
                }

                bool should_encode = vad_.is_speaking() || speech_hangover_frames_ > 0;

                if (should_encode) {
                    if (!speech_gate_active_) {
                        for (auto& frame : speech_pre_roll_) {
                            process_and_encode_frame(frame.data());
                        }
                        speech_pre_roll_.clear();
                    }
                    process_and_encode_frame(capture_accum_.data());
                    speech_gate_active_ = true;
                } else {
                    if (speech_gate_active_) {
                        vad_.force_silence();
                    }
                    speech_gate_active_ = false;

                    std::array<int16_t, FRAME_SIZE> frame{};
                    std::copy_n(capture_accum_.data(), FRAME_SIZE, frame.data());
                    speech_pre_roll_.push_back(frame);
                    while (speech_pre_roll_.size() > SPEECH_PRE_ROLL_FRAMES) {
                        speech_pre_roll_.pop_front();
                    }

                    // Track ambient floor only while closed so the speech threshold adapts
                    // without chasing active speech.
                    noise_floor_rms_ = 0.98f * noise_floor_rms_ + 0.02f * gate_rms;
                }
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
        MELLO_LOG_WARN("pipeline", "get_packet DROP: seq=%u pkt=%d > buf=%d",
                       pkt.sequence, total_size, buffer_size);
        outgoing_.pop();
        return 0;
    }
    uint32_t seq = pkt.sequence;
    buffer[0] = static_cast<uint8_t>(seq);
    buffer[1] = static_cast<uint8_t>(seq >> 8);
    buffer[2] = static_cast<uint8_t>(seq >> 16);
    buffer[3] = static_cast<uint8_t>(seq >> 24);
    std::memcpy(buffer + 4, pkt.data.data(), payload_size);
    outgoing_.pop();

    get_pkt_ctr_++;
    if (get_pkt_ctr_ <= 20 || (get_pkt_ctr_ % 500) == 0) {
        MELLO_LOG_DEBUG("pipeline", "get_packet: #%u seq=%u size=%d queue_left=%zu",
                        get_pkt_ctr_, seq, total_size, outgoing_.size());
    }
    return total_size;
}

void AudioPipeline::feed_packet(const char* peer_id, const uint8_t* data, int size) {
    if (deafened_ || !initialized_ || size < 4) {
        return;
    }

    rtp_recv_total_.fetch_add(1, std::memory_order_relaxed);
    std::string pid(peer_id);

    uint32_t seq = static_cast<uint32_t>(data[0]) |
                   (static_cast<uint32_t>(data[1]) << 8) |
                   (static_cast<uint32_t>(data[2]) << 16) |
                   (static_cast<uint32_t>(data[3]) << 24);

    // Ensure per-peer decoder, jitter buffer, and ring buffer exist.
    // All three maps are guarded by peer_buffers_mutex_ since mix_output()
    // on the audio device thread also iterates them.
    {
        std::lock_guard<std::mutex> lock(peer_buffers_mutex_);
        if (decoders_.find(pid) == decoders_.end()) {
            MELLO_LOG_INFO("pipeline", "creating decoder for peer '%s'", peer_id);
            auto& dec = decoders_[pid];
            if (!dec.initialize()) {
                MELLO_LOG_ERROR("pipeline", "opus decoder init failed for '%s'", peer_id);
                decoders_.erase(pid);
                return;
            }
            decoder_primed_[pid] = false;
            last_decoded_seq_.erase(pid);
        }
        if (peer_buffers_.find(pid) == peer_buffers_.end()) {
            peer_buffers_[pid] = std::make_unique<util::RingBuffer<int16_t>>(48000);
            active_streams_.store(static_cast<int>(peer_buffers_.size()), std::memory_order_relaxed);
            MELLO_LOG_INFO("pipeline", "created playback buffer for peer '%s' (streams=%d)",
                           peer_id, (int)peer_buffers_.size());
        }
        // Push into jitter buffer — device thread drives the pop.
        jitter_buffers_[pid].push(seq, data + 4, size - 4);
    }

    if ((seq % 500) == 0) {
        std::lock_guard<std::mutex> lock2(peer_buffers_mutex_);
        auto jit = jitter_buffers_.find(pid);
        int buf_count = jit != jitter_buffers_.end() ? jit->second.buffered_count() : 0;
        MELLO_LOG_DEBUG("pipeline", "feed(%s): seq=%u jitter_buf=%d",
                        peer_id, seq, buf_count);
    }
}

size_t AudioPipeline::mix_output(int16_t* out, size_t count) {
    std::lock_guard<std::mutex> lock(peer_buffers_mutex_);

    // Drain jitter buffers into ring buffers. Cap per-peer drain work per
    // callback so a backlog on one stream doesn't monopolize the audio lock.
    constexpr uint32_t kMaxConcealFramesPerPacket = 3;
    constexpr int kMaxDrainPacketsPerPeer = 6;
    int total_popped = 0;
    int total_decoded = 0;
    int decode_errors = 0;
    int concealment_fec = 0;
    int concealment_plc = 0;
    for (auto& [pid, jb] : jitter_buffers_) {
        std::vector<uint8_t> pkt_data;
        uint32_t pkt_seq = 0;
        int drained = 0;
        while (drained < kMaxDrainPacketsPerPeer) {
            auto pop_result = jb.pop(pkt_data, &pkt_seq);
            if (pop_result == JitterPopResult::None) {
                break;
            }
            drained++;
            auto dit = decoders_.find(pid);
            if (dit == decoders_.end()) continue;
            auto bit = peer_buffers_.find(pid);
            if (bit == peer_buffers_.end()) continue;

            if (pop_result == JitterPopResult::Missing) {
                if (decoder_primed_[pid]) {
                    int16_t plc_pcm[FRAME_SIZE];
                    int plc_samples = dit->second.decode_plc(plc_pcm, FRAME_SIZE);
                    if (plc_samples > 0) {
                        bit->second->write(plc_pcm, static_cast<size_t>(plc_samples));
                        concealment_plc++;
                        total_decoded++;
                    }
                }
                continue;
            }

            total_popped++;
            bool primed = decoder_primed_[pid];
            auto last_it = last_decoded_seq_.find(pid);
            uint32_t missing_frames = 0;
            if (last_it != last_decoded_seq_.end() && pkt_seq > last_it->second + 1) {
                missing_frames = pkt_seq - last_it->second - 1;
            }

            if (primed && missing_frames > 0) {
                bool used_fec = false;
                // With one-frame loss, try in-band FEC from the current packet first.
                if (missing_frames == 1) {
                    int16_t fec_pcm[FRAME_SIZE];
                    int fec_samples = dit->second.decode_fec(
                        pkt_data.data(), static_cast<int>(pkt_data.size()), fec_pcm, FRAME_SIZE);
                    if (fec_samples > 0) {
                        bit->second->write(fec_pcm, static_cast<size_t>(fec_samples));
                        concealment_fec++;
                        total_decoded++;
                        used_fec = true;
                    }
                }

                if (!used_fec) {
                    uint32_t plc_frames =
                        std::min<uint32_t>(missing_frames, kMaxConcealFramesPerPacket);
                    for (uint32_t i = 0; i < plc_frames; ++i) {
                        int16_t plc_pcm[FRAME_SIZE];
                        int plc_samples = dit->second.decode_plc(plc_pcm, FRAME_SIZE);
                        if (plc_samples <= 0) break;
                        bit->second->write(plc_pcm, static_cast<size_t>(plc_samples));
                        concealment_plc++;
                        total_decoded++;
                    }
                }
            }

            int16_t pcm[FRAME_SIZE];
            int samples = dit->second.decode(pkt_data.data(),
                                             static_cast<int>(pkt_data.size()),
                                             pcm, FRAME_SIZE);
            if (samples > 0) {
                total_decoded++;
                bit->second->write(pcm, static_cast<size_t>(samples));
                decoder_primed_[pid] = true;
                last_decoded_seq_[pid] = pkt_seq;
            } else {
                decode_errors++;
            }
        }
    }

    std::memset(out, 0, count * sizeof(int16_t));
    bool any_remote = false;

    if (!peer_buffers_.empty()) {
        std::vector<int16_t> temp(count);
        for (auto& [pid, buf] : peer_buffers_) {
            size_t got = buf->read(temp.data(), count);
            if (got < count && decoder_primed_[pid]) {
                auto dit = decoders_.find(pid);
                if (dit != decoders_.end()) {
                    size_t write_pos = got;
                    while (write_pos < count) {
                        int16_t plc_pcm[FRAME_SIZE];
                        int plc_samples = dit->second.decode_plc(plc_pcm, FRAME_SIZE);
                        if (plc_samples <= 0) break;
                        size_t copy_n = (std::min)(static_cast<size_t>(plc_samples), count - write_pos);
                        std::memcpy(temp.data() + write_pos, plc_pcm, copy_n * sizeof(int16_t));
                        write_pos += copy_n;
                        concealment_plc++;
                        total_decoded++;
                    }
                    got = write_pos;
                }
            }
            if (got > 0) {
                any_remote = true;
                for (size_t i = 0; i < got; ++i) {
                    int32_t mixed = static_cast<int32_t>(out[i]) + static_cast<int32_t>(temp[i]);
                    if (mixed > 32767) mixed = 32767;
                    if (mixed < -32768) mixed = -32768;
                    out[i] = static_cast<int16_t>(mixed);
                }
            }
        }
    }

    if (any_remote) {
        static uint32_t mix_log_ctr = 0;
        if ((mix_log_ctr++ % 500) == 0) {
            double sum = 0.0;
            for (size_t i = 0; i < count; ++i) {
                double s = out[i] / 32768.0;
                sum += s * s;
            }
            float rms = static_cast<float>(std::sqrt(sum / count));
            MELLO_LOG_INFO("pipeline", "mix_output #%u: rms=%.4f count=%zu peers=%zu popped=%d decoded=%d fec=%d plc=%d errs=%d",
                           mix_log_ctr, rms, count, peer_buffers_.size(), total_popped, total_decoded, concealment_fec, concealment_plc, decode_errors);
        }

        float og = output_gain_.load(std::memory_order_relaxed);
        if (og != 1.0f) {
            for (size_t i = 0; i < count; ++i) {
                int32_t s = static_cast<int32_t>(out[i] * og);
                if (s > 32767) s = 32767;
                if (s < -32768) s = -32768;
                out[i] = static_cast<int16_t>(s);
            }
        }
    } else {
        uint32_t ur = underrun_count_.fetch_add(1, std::memory_order_relaxed) + 1;
        if (!peer_buffers_.empty() && (ur <= 10 || (ur % 500) == 0)) {
            int total_jb = 0;
            int total_rb = 0;
            for (auto& [pid, jb] : jitter_buffers_) total_jb += jb.buffered_count();
            for (auto& [pid, buf] : peer_buffers_) total_rb += static_cast<int>(buf->available());
            MELLO_LOG_INFO("pipeline", "mix_output UNDERRUN #%u: peers=%zu jb_pkts=%d rb_samples=%d popped=%d decoded=%d fec=%d plc=%d errs=%d",
                           ur, peer_buffers_.size(), total_jb, total_rb, total_popped, total_decoded, concealment_fec, concealment_plc, decode_errors);
        }
        // Windowed underrun rate: warn when underruns are *currently* sustained
        // rather than relying on the ever-growing lifetime counter, which looks
        // alarming on long sessions even when audio is presently fine.
        if (!peer_buffers_.empty()) {
            int64_t now_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                                 std::chrono::steady_clock::now().time_since_epoch())
                                 .count();
            constexpr int64_t kUnderrunWindowMs = 5000;
            if (underrun_window_start_ms_ == 0) underrun_window_start_ms_ = now_ms;
            ++underrun_window_count_;
            int64_t elapsed = now_ms - underrun_window_start_ms_;
            if (elapsed >= kUnderrunWindowMs) {
                // ~5 underruns/sec sustained over the window => audibly degraded.
                if (underrun_window_count_ >= 25 &&
                    now_ms - last_underrun_warn_ms_ >= kUnderrunWindowMs) {
                    MELLO_LOG_WARN("pipeline",
                                   "sustained audio underruns: %d in last %lldms (%.1f/s) -- output likely degraded",
                                   underrun_window_count_, (long long)elapsed,
                                   underrun_window_count_ * 1000.0 / elapsed);
                    last_underrun_warn_ms_ = now_ms;
                }
                underrun_window_start_ms_ = now_ms;
                underrun_window_count_ = 0;
            }
        }
    }

    // Clip buffer: mix remote playback + local mic for "all participants" recording
    if (clip_buffer_ && clip_buffer_->is_active()) {
        if (local_clip_ring_ && local_clip_ring_->available() >= count) {
            std::vector<int16_t> local(count);
            local_clip_ring_->read(local.data(), count);
            std::vector<int16_t> clip_mix(count);
            for (size_t i = 0; i < count; ++i) {
                int32_t mixed = static_cast<int32_t>(out[i]) + static_cast<int32_t>(local[i]);
                if (mixed > 32767) mixed = 32767;
                if (mixed < -32768) mixed = -32768;
                clip_mix[i] = static_cast<int16_t>(mixed);
            }
            clip_buffer_->write(clip_mix.data(), count);
        } else {
            clip_buffer_->write(out, count);
        }
    }

    // Mix clip playback into speaker output (read directly from retained PCM vector)
    bool has_clip_audio = false;
    if (clip_playing_.load(std::memory_order_relaxed) &&
        !clip_paused_.load(std::memory_order_relaxed)) {
        size_t pos = clip_playback_pos_.load(std::memory_order_relaxed);
        size_t total = clip_playback_total_;
        size_t avail = (pos < total) ? (total - pos) : 0;
        size_t to_read = (std::min)(count, avail);
        if (to_read > 0) {
            has_clip_audio = true;
            for (size_t i = 0; i < to_read; ++i) {
                int32_t mixed = static_cast<int32_t>(out[i]) +
                                static_cast<int32_t>(clip_playback_pcm_[pos + i]);
                if (mixed > 32767) mixed = 32767;
                if (mixed < -32768) mixed = -32768;
                out[i] = static_cast<int16_t>(mixed);
            }
            clip_playback_pos_.store(pos + to_read, std::memory_order_relaxed);
        }
        if (pos + to_read >= total) {
            clip_playing_.store(false, std::memory_order_relaxed);
            MELLO_LOG_INFO("pipeline", "clip playback finished");
        }
    }

    // Feed remote playback to AEC as far-end reference
    if (any_remote || has_clip_audio) {
        echo_canceller_.process_render(out, static_cast<int>(count));
    }

    // Same signal conditions the neural suppressor's reference ring.
    if (echo_suppressor_.enabled() && (any_remote || has_clip_audio)) {
        echo_suppressor_.feed_far_end(out, count);
    }

    return (any_remote || has_clip_audio) ? count : 0;
}

float AudioPipeline::pipeline_delay_ms() const {
    std::lock_guard<std::mutex> lock(peer_buffers_mutex_);

    float jb_ms = 0.0f;
    int jb_count = 0;
    for (auto& [pid, jb] : jitter_buffers_) {
        jb_ms += jb.avg_hold_ms();
        jb_count++;
    }
    if (jb_count > 0) jb_ms /= static_cast<float>(jb_count);

    float pb_ms = 0.0f;
    for (auto& [pid, buf] : peer_buffers_) {
        pb_ms += static_cast<float>(buf->available()) / 48.0f;
    }
    if (!peer_buffers_.empty())
        pb_ms /= static_cast<float>(peer_buffers_.size());

    return jb_ms + pb_ms;
}

void AudioPipeline::refresh_stream_delay_hint() {
    if (!capture_ || !playback_) return;
    // Total render-to-capture latency ~= output device latency + input
    // device latency + jitter/playout buffering. The jitter term is small
    // at switch time (buffers just cleared); the AEC delay estimator
    // converges the rest. Sum, not difference: both legs add delay.
    // WASAPI latencies come from GetStreamLatency + GetDevicePeriod.
    // Sign check (sum vs difference) is validated by the ERLE harness
    // runs in the Windows handoff task 3.
    int out_ms = playback_->output_latency_ms();
    int in_ms = capture_->input_latency_ms();
    float jb_ms = 0.0f;
    if (initialized_) {
        jb_ms = pipeline_delay_ms();
    }
    int delay = out_ms + in_ms + static_cast<int>(jb_ms + 0.5f);
    if (delay < 0) delay = 0;
    if (delay > 500) delay = 500;
    echo_canceller_.set_stream_delay_ms(delay);
    MELLO_LOG_INFO("pipeline", "stream delay hint: out=%d in=%d jb=%.1f total=%d ms",
                   out_ms, in_ms, jb_ms, delay);
}

void AudioPipeline::clear_remote_streams() {
    std::lock_guard<std::mutex> lock(peer_buffers_mutex_);
    size_t had = peer_buffers_.size();
    decoders_.clear();
    decoder_primed_.clear();
    last_decoded_seq_.clear();
    jitter_buffers_.clear();
    peer_buffers_.clear();
    active_streams_.store(0, std::memory_order_relaxed);
    if (had > 0) {
        MELLO_LOG_INFO("pipeline", "cleared %zu remote voice streams", had);
    }
}

AudioDeviceEnumerator& AudioPipeline::device_enumerator() {
    if (!device_enum_) {
        device_enum_ = create_device_enumerator();
    }
    return *device_enum_;
}

int AudioPipeline::set_capture_device(const char* device_id) {
    MELLO_LOG_INFO("pipeline", "switching capture device (was_capturing=%d)", (int)capturing_.load());
    capture_device_id_ = device_id ? device_id : "";

#ifdef __APPLE__
    // Duplex mode rebuilds the whole pair (the unit spans both directions),
    // but only mid-session: outside voice there is nothing to restart and
    // no unit may run.
    if (voice_processing_capture_.load(std::memory_order_relaxed) && capturing_) {
        switch_audio_backend(true);
        if (!capture_ || !playback_) return 0;
        return capture_->provides_echo_cancellation() ? 1 : 2;
    }
#endif

    bool was_capturing = capturing_;
    if (was_capturing && capture_) {
        capture_->stop();
        capturing_ = false;
    }

    bool fell_back = false;
    bool init_ok = false;
    capture_ = create_audio_capture();
    try { init_ok = capture_->initialize(device_id); } catch (...) { init_ok = false; }
    if (!init_ok) {
        MELLO_LOG_WARN("pipeline", "capture device switch failed, falling back to default");
        capture_ = create_audio_capture();
        if (!capture_->initialize(nullptr)) {
            MELLO_LOG_ERROR("pipeline", "default capture device also failed");
            return 0;
        }
        fell_back = true;
    }

    if (was_capturing) {
        bool ok = capture_->start([this](const int16_t* samples, size_t count) {
            on_captured_audio(samples, count);
        });
        if (ok) capturing_ = true;
        MELLO_LOG_INFO("pipeline", "capture restarted on new device: %s", ok ? "ok" : "FAILED");
        if (ok) {
            refresh_stream_delay_hint();
            echo_suppressor_.reset();
        }
        return ok ? (fell_back ? 2 : 1) : 0;
    }
    refresh_stream_delay_hint();
    echo_suppressor_.reset();
    return fell_back ? 2 : 1;
}

int AudioPipeline::set_playback_device(const char* device_id) {
    MELLO_LOG_INFO("pipeline", "switching playback device");
    playback_device_id_ = device_id ? device_id : "";

#ifdef __APPLE__
    // Duplex mode rebuilds the whole pair (the unit spans both directions),
    // but only mid-session: outside voice there is nothing to restart and
    // no unit may run.
    if (voice_processing_capture_.load(std::memory_order_relaxed) && capturing_) {
        switch_audio_backend(true);
        if (!capture_ || !playback_) return 0;
        return capture_->provides_echo_cancellation() ? 1 : 2;
    }
#endif

    if (playback_) playback_->stop();

    bool fell_back = false;
    bool init_ok = false;
    playback_ = create_audio_playback();
#ifdef _WIN32
    apply_session(playback_.get());
#endif
    try { init_ok = playback_->initialize(device_id); } catch (...) { init_ok = false; }
    if (!init_ok) {
        MELLO_LOG_WARN("pipeline", "playback device switch failed, falling back to default");
        playback_ = create_audio_playback();
#ifdef _WIN32
        apply_session(playback_.get());
#endif
        if (!playback_->initialize(nullptr)) {
            MELLO_LOG_ERROR("pipeline", "default playback device also failed");
            return 0;
        }
        fell_back = true;
    }
    playback_->set_render_source([this](int16_t* out, size_t count) -> size_t {
        return mix_output(out, count);
    });
    bool ok = playback_->start();
    MELLO_LOG_INFO("pipeline", "playback restarted on new device: %s", ok ? "ok" : "FAILED");
    if (ok) {
        refresh_stream_delay_hint();
        echo_suppressor_.reset();
    }
    return ok ? (fell_back ? 2 : 1) : 0;
}

void AudioPipeline::start_clip_buffer() {
    if (!clip_buffer_) {
        clip_buffer_ = std::make_unique<ClipBuffer>(SAMPLE_RATE, 60);
    }
    if (!local_clip_ring_) {
        local_clip_ring_ = std::make_unique<util::RingBuffer<int16_t>>(SAMPLE_RATE);
    }
    local_clip_ring_->clear();
    clip_buffer_->start();
}

void AudioPipeline::stop_clip_buffer() {
    if (clip_buffer_) clip_buffer_->stop();
}

bool AudioPipeline::clip_buffer_active() const {
    return clip_buffer_ && clip_buffer_->is_active();
}

bool AudioPipeline::clip_capture(float seconds, const std::string& output_path) {
    if (!clip_buffer_) return false;
    return clip_buffer_->capture(seconds, output_path);
}

bool AudioPipeline::play_clip(const std::string& wav_path) {
    std::ifstream file(wav_path, std::ios::binary);
    if (!file) {
        MELLO_LOG_ERROR("pipeline", "play_clip: cannot open %s", wav_path.c_str());
        return false;
    }

    char header[44];
    file.read(header, 44);
    if (!file || std::string(header, 4) != "RIFF" || std::string(header + 8, 4) != "WAVE") {
        MELLO_LOG_ERROR("pipeline", "play_clip: not a valid WAV file");
        return false;
    }

    uint16_t channels = *reinterpret_cast<uint16_t*>(header + 22);
    uint32_t sr = *reinterpret_cast<uint32_t*>(header + 24);
    uint16_t bps = *reinterpret_cast<uint16_t*>(header + 34);
    uint32_t data_size = *reinterpret_cast<uint32_t*>(header + 40);

    if (bps != 16 || channels != 1 || sr != static_cast<uint32_t>(SAMPLE_RATE)) {
        MELLO_LOG_ERROR("pipeline", "play_clip: unsupported format (ch=%d sr=%u bps=%d, need mono %dHz 16-bit)",
                        channels, sr, bps, SAMPLE_RATE);
        return false;
    }

    size_t sample_count = data_size / sizeof(int16_t);
    clip_playback_pcm_.resize(sample_count);
    file.read(reinterpret_cast<char*>(clip_playback_pcm_.data()), data_size);
    clip_playback_total_ = sample_count;
    clip_playback_pos_.store(0, std::memory_order_release);
    clip_paused_.store(false, std::memory_order_release);
    clip_playing_.store(true, std::memory_order_release);

    MELLO_LOG_INFO("pipeline", "play_clip: loaded %zu samples (%.1fs) from %s",
                   sample_count, static_cast<float>(sample_count) / SAMPLE_RATE, wav_path.c_str());
    return true;
}

bool AudioPipeline::play_mp4(const std::string& mp4_path) {
    auto pcm = decode_mp4_to_pcm(mp4_path);
    if (pcm.empty()) {
        MELLO_LOG_ERROR("pipeline", "play_mp4: decode failed for %s", mp4_path.c_str());
        return false;
    }

    clip_playback_pcm_ = std::move(pcm);
    clip_playback_total_ = clip_playback_pcm_.size();
    clip_playback_pos_.store(0, std::memory_order_release);
    clip_paused_.store(false, std::memory_order_release);
    clip_playing_.store(true, std::memory_order_release);

    MELLO_LOG_INFO("pipeline", "play_mp4: loaded %zu samples (%.1fs) from %s",
                   clip_playback_pcm_.size(),
                   static_cast<float>(clip_playback_pcm_.size()) / SAMPLE_RATE,
                   mp4_path.c_str());
    return true;
}

void AudioPipeline::stop_clip_playback() {
    clip_playing_.store(false, std::memory_order_release);
    clip_paused_.store(false, std::memory_order_release);
    clip_playback_pos_.store(0, std::memory_order_relaxed);
    clip_playback_pcm_.clear();
    clip_playback_total_ = 0;
    MELLO_LOG_INFO("pipeline", "clip playback stopped");
}

bool AudioPipeline::clip_is_playing() const {
    return clip_playing_.load(std::memory_order_relaxed);
}

void AudioPipeline::clip_playback_progress(uint64_t& position_samples,
                                           uint64_t& total_samples,
                                           uint32_t& sample_rate) const {
    position_samples = clip_playback_pos_.load(std::memory_order_relaxed);
    total_samples = clip_playback_total_;
    sample_rate = SAMPLE_RATE;
}

void AudioPipeline::clip_pause() {
    clip_paused_.store(true, std::memory_order_release);
    MELLO_LOG_INFO("pipeline", "clip playback paused");
}

void AudioPipeline::clip_resume() {
    clip_paused_.store(false, std::memory_order_release);
    MELLO_LOG_INFO("pipeline", "clip playback resumed");
}

void AudioPipeline::clip_seek(uint64_t position_samples) {
    if (position_samples > clip_playback_total_) {
        position_samples = clip_playback_total_;
    }
    clip_playback_pos_.store(static_cast<size_t>(position_samples), std::memory_order_release);
    MELLO_LOG_INFO("pipeline", "clip playback seek to sample %llu / %zu",
                   (unsigned long long)position_samples, clip_playback_total_);
}

#ifdef _WIN32
void AudioPipeline::apply_session(AudioPlayback* pb) {
    if (!session_win_ || !pb) return;
    static_cast<WasapiPlayback*>(pb)->set_session(session_win_.get());
}
#endif

} // namespace mello::audio
