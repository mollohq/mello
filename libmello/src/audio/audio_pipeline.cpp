#include "audio_pipeline.hpp"
#include "audio_backend_test.hpp"
#include "../util/log.hpp"
#include "../util/test_clock.hpp"
#include <cstdlib>
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
#else
#include <unistd.h>
#include <limits.h>
#include <libgen.h>
#endif
#ifdef MELLO_HAS_VPIO_DUPLEX
#include "vpio_duplex.hpp"
#endif

namespace mello::audio {

#ifdef MELLO_HAS_VPIO_DUPLEX
// The desired capture backend for an echo cancellation setting. The duplex
// unit opens the real microphone and speaker through CoreAudio. The test
// backend (MELLO_AUDIO_BACKEND=test) opens no device, so it never gets the
// unit: the software APM runs, as on Windows.
static bool wants_vpio_duplex(bool aec_enabled) {
    return aec_enabled && !test_audio_backend_requested();
}
#endif

// Default speech gate pre-roll, chosen with the voice gate (TESTING.md,
// talk-spurt gate).
static constexpr int DEFAULT_SPEECH_PRE_ROLL_FRAMES = 5;
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

// NetEQ configuration. The voice quality gate may override the delay
// manager tuning (MELLO_NETEQ_DELAY_MANAGER_CONFIG), turn the talk-spurt
// hold off (MELLO_NETEQ_SPURT_HOLD=0), and set the hold floor
// (MELLO_NETEQ_SPURT_HOLD_FLOOR_MS) and the preemptive-expand window after a
// target jump (MELLO_NETEQ_JUMP_NO_EXPAND_MS) through the environment, with
// the test backend only, so a sweep needs no rebuild. Production always uses
// the defaults.
static VoicePlayout::Config playout_config() {
    VoicePlayout::Config c;
    c.delay_manager_config = VoicePlayout::default_delay_manager_config();
    if (test_audio_backend_requested()) {
        const std::string dm = test_backend_env("MELLO_NETEQ_DELAY_MANAGER_CONFIG");
        if (!dm.empty()) c.delay_manager_config = dm == "default" ? std::string() : dm;
        c.talk_spurt_hold = test_backend_env("MELLO_NETEQ_SPURT_HOLD") != "0";
        const std::string floor = test_backend_env("MELLO_NETEQ_SPURT_HOLD_FLOOR_MS");
        if (!floor.empty()) c.spurt_hold_floor_ms = std::atoi(floor.c_str());
        const std::string window = test_backend_env("MELLO_NETEQ_JUMP_NO_EXPAND_MS");
        if (!window.empty()) c.jump_no_expand_ms = std::atoi(window.c_str());
    }
    return c;
}

// Pre-roll length in frames. The voice quality gate may change it through the
// environment, with the test backend only, to sweep the delay against the
// onset loss. Production always starts with DEFAULT_SPEECH_PRE_ROLL_FRAMES.
static int pre_roll_frames_config() {
    if (test_audio_backend_requested()) {
        const std::string v = test_backend_env("MELLO_VOICE_PRE_ROLL_FRAMES");
        if (!v.empty()) {
            const int n = std::atoi(v.c_str());
            if (n >= 0 && n <= AudioPipeline::kMaxPreRollFrames) return n;
        }
    }
    return DEFAULT_SPEECH_PRE_ROLL_FRAMES;
}

AudioPipeline::AudioPipeline()
    : playout_(std::make_unique<VoicePlayout>(playout_config())),
      pre_roll_frames_(pre_roll_frames_config()) {}

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
    // The test backend opens no device, so it needs no ducking session.
    if (!test_audio_backend_requested()) {
        session_win_ = std::make_unique<AudioSessionWin>();
        session_win_->initialize();
    }
#endif

    capture_ = create_audio_capture();
    playback_ = create_audio_playback();
#ifdef _WIN32
    apply_session(playback_.get());
#endif
#ifdef MELLO_HAS_VPIO_DUPLEX
    // Voice-session scope: the duplex unit lights the mic at the device
    // level as soon as it starts, so it must not exist before a voice
    // session. Startup is always the plain pair (clips preview mic-free);
    // the toggle applies at capture start. Only the desired backend is
    // stored here.
    voice_processing_capture_.store(wants_vpio_duplex(echo_canceller_.aec_enabled()),
                                    std::memory_order_relaxed);
#endif
    if (!capture_->initialize(current_capture_device_id())) {
        MELLO_LOG_ERROR("pipeline", "capture init failed");
        return false;
    }

#ifdef _WIN32
    apply_session(playback_.get());
#endif
    request_stereo_output(playback_.get());
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
    wire_render_source();

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
    set_packet_sink(nullptr);
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
    // The send leg of the new session is unknown: start at the top rung.
    reset_send_adaptation();

#ifdef MELLO_HAS_VPIO_DUPLEX
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

#ifdef MELLO_HAS_VPIO_DUPLEX
    // Leaving voice releases the mic: drop a live duplex unit back to the
    // plain pair. Desired stays where the toggle put it (the APM flag
    // mirrors it); only the live backend drops. Skipped while dying.
    if (initialized_ && capture_ && capture_->provides_echo_cancellation()) {
        switch_audio_backend(false);
        voice_processing_capture_.store(wants_vpio_duplex(echo_canceller_.aec_enabled()),
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

    reset_send_adaptation();
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
#ifdef MELLO_HAS_VPIO_DUPLEX
    // On macOS the toggle selects the capture backend: on = combined VPIO
    // duplex (OS AEC/AGC, our APM capture pass skipped), off = plain HAL
    // pair + software AEC. Elsewhere it only flips the APM flag. The
    // switch applies immediately only mid-session; outside voice the
    // desired backend is stored for the next capture start, so no unit
    // ever runs (and no mic shows) without a session.
    const bool duplex = wants_vpio_duplex(enabled);
    if (initialized_ && capturing_ &&
        voice_processing_capture_.load(std::memory_order_relaxed) != duplex) {
        switch_audio_backend(duplex);
    } else {
        voice_processing_capture_.store(duplex, std::memory_order_relaxed);
    }
#endif
}

#ifdef MELLO_HAS_VPIO_DUPLEX
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
    request_stereo_output(playback_.get());
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
#ifdef MELLO_HAS_VPIO_DUPLEX
    if (voice_processing) live = activate_vpio_pair();
    if (!live) {
        if (voice_processing) {
            MELLO_LOG_WARN("pipeline", "VPIO duplex unavailable, falling back to plain HAL pair");
        }
        activate_plain_pair();
    }
#else
    // Only reached with the VPIO backend (call sites are guarded); plain pair it is.
    (void)voice_processing;
    (void)live;
    capture_ = create_audio_capture();
    playback_ = create_audio_playback();
#ifdef _WIN32
    apply_session(playback_.get());
#endif
    request_stereo_output(playback_.get());
#endif

    // (Re)wire both halves before start (device-switch invariants): the
    // capture callback and the render source do not transfer across
    // replacement instances.
    if (!capture_->initialize(current_capture_device_id()) ||
        !playback_->initialize(current_playback_device_id())) {
        MELLO_LOG_WARN("pipeline", "backend switch failed on stored devices, retrying defaults");
        capture_device_id_.clear();
        playback_device_id_.clear();
#ifdef MELLO_HAS_VPIO_DUPLEX
        activate_plain_pair();
#else
        capture_ = create_audio_capture();
        playback_ = create_audio_playback();
#ifdef _WIN32
        apply_session(playback_.get());
#endif
        request_stereo_output(playback_.get());
#endif
        if (!capture_->initialize(nullptr) || !playback_->initialize(nullptr)) {
            MELLO_LOG_ERROR("pipeline", "backend switch failed on fallback too");
            return;
        }
    }
    wire_render_source();
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

void AudioPipeline::set_output_volume(float vol) {
    output_gain_.store(vol, std::memory_order_relaxed);
    // Applied in each mixer source, before the limiter (spec 10 section 5.3).
    playout_->set_output_gain(vol);
}
void AudioPipeline::set_deafen(bool deafened) { deafened_ = deafened; }

void AudioPipeline::reset_speech_gate_state() {
    speech_pre_roll_.clear();
    candidate_hangover_frames_ = 0;
    speech_hangover_frames_ = 0;
    speech_gate_active_ = false;
    vad_fed_last_frame_ = false;
    prev_frame_.fill(0);
    speech_expander_.reset();
    vad_.force_silence();
}

void AudioPipeline::set_speech_pre_roll_frames(int frames) {
    frames = (std::min)(kMaxPreRollFrames, (std::max)(0, frames));
    std::lock_guard<std::mutex> lock(accum_mutex_);
    pre_roll_frames_.store(frames, std::memory_order_relaxed);
    while (speech_pre_roll_.size() > static_cast<size_t>(frames)) {
        speech_pre_roll_.pop_front();
    }
    MELLO_LOG_INFO("pipeline", "speech pre-roll %d frames", frames);
}

void AudioPipeline::set_encode_tap(EncodeTap tap) {
    std::lock_guard<std::mutex> lock(accum_mutex_);
    encode_tap_ = std::move(tap);
}

void AudioPipeline::set_input_sensitivity(float db) {
    if (!std::isfinite(db)) return;
    db = (std::min)(0.0f, (std::max)(-100.0f, db));
    sensitivity_db_.store(db, std::memory_order_relaxed);
    MELLO_LOG_INFO("pipeline", "input sensitivity set to %.1f dBFS", db);
}

void AudioPipeline::set_input_sensitivity_auto(bool enabled) {
    sensitivity_auto_.store(enabled, std::memory_order_relaxed);
    MELLO_LOG_INFO("pipeline", "input sensitivity %s", enabled ? "auto" : "manual");
}

float AudioPipeline::input_gate_dbfs() const {
    return 20.0f * std::log10((std::max)(gate_threshold_rms_.load(std::memory_order_relaxed), 1e-10f));
}

void AudioPipeline::set_push_to_talk(bool enabled) {
    push_to_talk_mode_.store(enabled, std::memory_order_relaxed);
    if (enabled) {
        std::lock_guard<std::mutex> lock(accum_mutex_);
        reset_speech_gate_state();
    }
    MELLO_LOG_INFO("pipeline", "push_to_talk mode %s", enabled ? "enabled" : "disabled");
}

void AudioPipeline::apply_send_adaptation() {
    const int bitrate = adapt_bitrate_bps_.load(std::memory_order_relaxed);
    if (bitrate != applied_bitrate_bps_) {
        encoder_.set_bitrate(bitrate);
        applied_bitrate_bps_ = bitrate;
    }
    const int loss_perc = adapt_loss_perc_.load(std::memory_order_relaxed);
    if (loss_perc != applied_loss_perc_) {
        encoder_.set_packet_loss_perc(loss_perc);
        applied_loss_perc_ = loss_perc;
    }
}

void AudioPipeline::reset_send_adaptation() {
    std::lock_guard<std::mutex> lock(adapt_mutex_);
    adapt_.reset();
    last_feedback_ = MelloSendFeedback{};
    session_reports_ = 0;
    adapt_bitrate_bps_.store(adapt_.bitrate_kbps() * 1000, std::memory_order_relaxed);
    adapt_loss_perc_.store(adapt_.loss_perc(), std::memory_order_relaxed);
}

void AudioPipeline::report_send_feedback(const MelloSendFeedback& feedback) {
    std::lock_guard<std::mutex> lock(adapt_mutex_);
    last_feedback_ = feedback;
    ++session_reports_;
    // A repeated LSR repeats an old forward delay: no round-trip sample.
    adapt_.on_report(util::steady_now_ms(), feedback.interval_expected,
                     feedback.interval_lost, feedback.rtt_fresh ? feedback.rtt_ms : -1.0f,
                     feedback.jitter_ms);
    adapt_bitrate_bps_.store(adapt_.bitrate_kbps() * 1000, std::memory_order_relaxed);
    adapt_loss_perc_.store(adapt_.loss_perc(), std::memory_order_relaxed);
}

void AudioPipeline::set_max_bitrate_kbps(int kbps) {
    std::lock_guard<std::mutex> lock(adapt_mutex_);
    adapt_.set_max_kbps(kbps);
    adapt_bitrate_bps_.store(adapt_.bitrate_kbps() * 1000, std::memory_order_relaxed);
}

SendStats AudioPipeline::send_stats() const {
    std::lock_guard<std::mutex> lock(adapt_mutex_);
    SendStats s;
    s.feedback = last_feedback_;
    s.reports = session_reports_;
    s.bitrate_kbps = adapt_.bitrate_kbps();
    s.max_bitrate_kbps = adapt_.max_kbps();
    s.loss_perc = adapt_.loss_perc();
    s.rung_changes = adapt_.rung_changes();
    s.window_loss_pct = adapt_.window_loss() < 0.0
                            ? -1.0f
                            : static_cast<float>(adapt_.window_loss() * 100.0);
    return s;
}

void AudioPipeline::process_and_encode_frame(int16_t* frame, uint32_t timestamp) {
    if (ns_mode() == NsMode::Rnnoise) {
        noise_suppressor_.process(frame, FRAME_SIZE);
    }

    if (encode_tap_) encode_tap_(frame, FRAME_SIZE, timestamp);

    apply_send_adaptation();
    uint8_t raw_pkt[MAX_PACKET_SIZE];
    int encoded = encoder_.encode(frame, FRAME_SIZE, raw_pkt, MAX_PACKET_SIZE);

    if (encoded > 0) {
        const uint32_t seq = sequence_++;
        std::lock_guard<std::mutex> slock(packet_sink_mutex_);
        if (packet_sink_) {
            // Direct send: the frame leaves on this thread, with no wait
            // for a consumer of the queue (spec 10 section 4.5).
            packet_sink_(raw_pkt, encoded, timestamp, seq);
            if ((seq % 250) == 0 || seq < 5) {
                MELLO_LOG_DEBUG("pipeline", "encode: seq=%u size=%d bytes, vad=%.2f, to sink",
                                seq, encoded, vad_.probability());
            }
            return;
        }
        std::lock_guard<std::mutex> olock(outgoing_mutex_);
        EncodedPacket pkt;
        pkt.data.assign(raw_pkt, raw_pkt + encoded);
        pkt.sequence = seq;
        pkt.timestamp = timestamp;
        outgoing_.push(std::move(pkt));

        if ((seq % 250) == 0 || seq < 5) {
            MELLO_LOG_DEBUG("pipeline", "encode: seq=%u size=%d bytes, vad=%.2f, queue=%zu",
                            seq, encoded, vad_.probability(), outgoing_.size());
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
        // Media time of this frame, counted whether it is encoded or not.
        const uint32_t frame_ts = capture_timestamp_;
        capture_timestamp_ += FRAME_SIZE;

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

        const bool muted = muted_.load(std::memory_order_relaxed);
        if (!muted) {
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
                vad_fed_last_frame_ = false;
                process_and_encode_frame(capture_accum_.data(), frame_ts);
            } else {
                // Auto: track the ambient floor. Manual: the user's level,
                // in dBFS of the raw frame RMS (the input meter's scale).
                const float speech_threshold =
                    sensitivity_auto_.load(std::memory_order_relaxed)
                        ? (std::max)(MIN_SPEECH_RMS, noise_floor_rms_ * NOISE_FLOOR_GATE_MULT)
                        : std::pow(10.0f, sensitivity_db_.load(std::memory_order_relaxed) / 20.0f);
                gate_threshold_rms_.store(speech_threshold, std::memory_order_relaxed);
                bool candidate_speech = gate_rms >= speech_threshold;
                if (candidate_speech) {
                    gate_candidate_frames_.fetch_add(1, std::memory_order_relaxed);
                }

                if (candidate_speech) {
                    candidate_hangover_frames_ = CANDIDATE_HANGOVER_FRAMES;
                } else if (candidate_hangover_frames_ > 0) {
                    candidate_hangover_frames_--;
                }

                bool should_run_vad = candidate_speech || candidate_hangover_frames_ > 0 ||
                                      speech_hangover_frames_ > 0;

                if (should_run_vad) {
                    // Silero decides per 32 ms chunk (spec 10 section 4.1).
                    // When it starts again after the RMS gate kept it idle,
                    // its first chunk is the end of the previous frame and
                    // this frame, so it ends with this frame. The samples
                    // left from the last run are old audio and are dropped.
                    // While the gate waits for speech, a frame in which no
                    // chunk completed gets a decision on its last 32 ms
                    // (peek), so the gate does not open one frame late.
                    if (!vad_fed_last_frame_) {
                        vad_.restart(prev_frame_.data(), FRAME_SIZE,
                                     VAD_CHUNK_SIZE - FRAME_SIZE / 3);
                    }
                    const int chunks = vad_.feed(capture_accum_.data(), FRAME_SIZE);
                    if (chunks == 0 && candidate_speech && !vad_.is_speaking()) {
                        vad_.peek();
                    }
                }
                vad_fed_last_frame_ = should_run_vad;
                std::copy_n(capture_accum_.data(), FRAME_SIZE, prev_frame_.data());

                if (vad_.is_speaking()) {
                    speech_hangover_frames_ = SPEECH_ENCODE_HANGOVER_FRAMES;
                } else if (speech_hangover_frames_ > 0) {
                    speech_hangover_frames_--;
                }

                bool should_encode = vad_.is_speaking() || speech_hangover_frames_ > 0;
                const bool gate_opens = should_encode && !speech_gate_active_;

                // Pre-roll (spec 10 section 4.1): keep the frame as captured,
                // before the expander. These frames were captured while
                // Silero said "no speech"; when the gate opens they are the
                // start of the word and leave at their captured level.
                if (!should_encode) {
                    PreRollFrame frame{};
                    frame.timestamp = frame_ts;
                    std::copy_n(capture_accum_.data(), FRAME_SIZE, frame.pcm.data());
                    speech_pre_roll_.push_back(frame);
                    const size_t keep = static_cast<size_t>(
                        pre_roll_frames_.load(std::memory_order_relaxed));
                    while (speech_pre_roll_.size() > keep) {
                        speech_pre_roll_.pop_front();
                    }
                }

                // Downward expander (post-AGC): attenuate the frame toward its
                // floor when the VAD says no speech, so AGC gain ramped for a
                // quiet talker cannot pump the noise floor in gaps. Smoothed
                // release keeps word tails; the gate decision above is
                // unchanged (it read capture_accum_ before this point). It
                // acts on live frames only: the pre-roll copy above is taken
                // before it.
                //
                // Keyed off the RAW VAD probability, not is_speaking(): the
                // latter latches true through an 8-frame holdover, so it stays
                // set through the inter-word gaps where the floor is pumped
                // (observed in ostkatt's log: prob 0.04 but is_speaking still
                // true). The raw probability drops in those gaps and closes the
                // expander there.
                //
                // When the gate opens, the expander starts at unity: the
                // pre-roll in front of this frame leaves at unity, and an
                // attack from the floor would put a dip of about 4 dB right
                // after the start of the word.
                if (gate_opens) {
                    speech_expander_.reset();
                }
                const bool expander_speech =
                    vad_.probability() >= VAD_THRESHOLD;
                speech_expander_.process(capture_accum_.data(), FRAME_SIZE,
                                         expander_speech);

                if (should_encode) {
                    if (gate_opens) {
                        for (auto& frame : speech_pre_roll_) {
                            process_and_encode_frame(frame.pcm.data(), frame.timestamp);
                        }
                        speech_pre_roll_.clear();
                    }
                    process_and_encode_frame(capture_accum_.data(), frame_ts);
                    speech_gate_active_ = true;
                } else {
                    if (speech_gate_active_) {
                        vad_.force_silence();
                    }
                    speech_gate_active_ = false;

                    // Track ambient floor only while closed so the speech threshold adapts
                    // without chasing active speech.
                    noise_floor_rms_ = 0.98f * noise_floor_rms_ + 0.02f * gate_rms;
                }
            }
        }
        if (muted) {
            vad_fed_last_frame_ = false;
        }
        capture_accum_.erase(capture_accum_.begin(),
                             capture_accum_.begin() + FRAME_SIZE);
    }
}

void AudioPipeline::set_packet_sink(PacketSink sink) {
    const bool set = static_cast<bool>(sink);
    // Waits for a sink call that runs now on the capture thread.
    std::lock_guard<std::mutex> slock(packet_sink_mutex_);
    const bool was_set = static_cast<bool>(packet_sink_);
    packet_sink_ = std::move(sink);
    size_t dropped = 0;
    if (set) {
        // Frames queued before the sink would leave late, or reach the
        // next get_packet caller as stale audio.
        std::lock_guard<std::mutex> olock(outgoing_mutex_);
        dropped = outgoing_.size();
        std::queue<EncodedPacket>().swap(outgoing_);
    }
    if (set || was_set) {
        MELLO_LOG_INFO("pipeline", "packet sink %s (queued frames dropped=%zu)",
                       set ? (was_set ? "replaced" : "set") : "cleared", dropped);
    }
}

int AudioPipeline::get_packet(uint8_t* buffer, int buffer_size, uint32_t* out_timestamp) {
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
    if (out_timestamp) {
        *out_timestamp = pkt.timestamp;
    }
    outgoing_.pop();

    get_pkt_ctr_++;
    if (get_pkt_ctr_ <= 20 || (get_pkt_ctr_ % 500) == 0) {
        MELLO_LOG_DEBUG("pipeline", "get_packet: #%u seq=%u size=%d queue_left=%zu",
                        get_pkt_ctr_, seq, total_size, outgoing_.size());
    }
    return total_size;
}

void AudioPipeline::feed_packet(const char* peer_id, const uint8_t* data, int size) {
    if (!initialized_ || !peer_id || !data || size <= VOICE_FRAME_HEADER_SIZE) {
        return;
    }
    rtp_recv_total_.fetch_add(1, std::memory_order_relaxed);
    auto le32 = [](const uint8_t* p) {
        return static_cast<uint32_t>(p[0]) | (static_cast<uint32_t>(p[1]) << 8) |
               (static_cast<uint32_t>(p[2]) << 16) | (static_cast<uint32_t>(p[3]) << 24);
    };
    // The RTP fields of the voice frame (spec 10 section 4.2). NetEQ keys on
    // the payload type and the timestamp; the 16-bit sequence wraps like RTP.
    RtpInfo rtp;
    rtp.payload_type = VoicePlayout::kOpusPayloadType;
    rtp.sequence = static_cast<uint16_t>(le32(data));
    rtp.timestamp = le32(data + 4);
    rtp.ssrc = 0;
    playout_->insert(peer_id, rtp, data + VOICE_FRAME_HEADER_SIZE,
                     static_cast<size_t>(size - VOICE_FRAME_HEADER_SIZE), util::steady_now_ms());
}

void AudioPipeline::feed_rtp(const char* peer_id, const uint8_t* packet, int size,
                             int64_t receive_time_ms) {
    if (!initialized_ || !peer_id || !packet || size <= 0) {
        return;
    }
    rtp_recv_total_.fetch_add(1, std::memory_order_relaxed);
    playout_->insert_rtp(peer_id, packet, static_cast<size_t>(size),
                         receive_time_ms < 0 ? util::steady_now_ms() : receive_time_ms);
}

void AudioPipeline::remove_peer(const char* peer_id) {
    if (!peer_id) return;
    playout_->remove_peer(peer_id);
}

void AudioPipeline::set_peer_volume(const char* peer_id, float gain) {
    if (!peer_id) return;
    playout_->set_peer_volume(peer_id, gain);
    MELLO_LOG_INFO("pipeline", "peer '%s' volume %.2f", peer_id, playout_->peer_volume(peer_id));
}

void AudioPipeline::set_peer_pan(const char* peer_id, float pan) {
    if (!peer_id) return;
    playout_->set_peer_pan(peer_id, pan);
    MELLO_LOG_INFO("pipeline", "peer '%s' pan %.2f", peer_id, playout_->peer_pan(peer_id));
}

float AudioPipeline::peer_volume(const char* peer_id) const {
    return peer_id ? playout_->peer_volume(peer_id) : 1.0f;
}

float AudioPipeline::peer_pan(const char* peer_id) const {
    return peer_id ? playout_->peer_pan(peer_id) : 0.0f;
}

void AudioPipeline::request_stereo_output(AudioPlayback* pb) {
    if (pb) pb->set_input_channels(VoicePlayout::kChannels);
}

void AudioPipeline::wire_render_source() {
    // The channel count is fixed for the life of this backend instance, so
    // the device thread never reads playback_ (a device switch replaces it).
    const uint32_t channels = playback_->render_channels();
    playback_->set_render_source([this, channels](int16_t* out, size_t count) -> size_t {
        return mix_output(out, count, channels);
    });
    MELLO_LOG_INFO("pipeline", "voice output: %u channel(s)", channels);
}

void AudioPipeline::render_frame() {
    constexpr size_t kFrames = VoicePlayout::kFrameSamples;
    int16_t* stereo = ring_.data();

    // NetEQ GetAudio for every peer and the mix, also while deafened, so each
    // peer's timeline and delay estimate keep running.
    const int sources = playout_->mix(stereo);
    bool remote = sources > 0;
    if (remote && deafened_.load(std::memory_order_relaxed)) {
        std::fill(ring_.begin(), ring_.end(), int16_t{0});
        remote = false;
    }

    // What the speakers play, as one channel: the clip tap and the echo
    // canceller reference. A centred peer gives L = R, so this is exact.
    for (size_t i = 0; i < kFrames; ++i) {
        mono_[i] = static_cast<int16_t>(
            (static_cast<int32_t>(stereo[2 * i]) + static_cast<int32_t>(stereo[2 * i + 1])) / 2);
    }

    if (!remote) {
        const uint32_t ur =
            static_cast<uint32_t>(underrun_count_.fetch_add(1, std::memory_order_relaxed)) + 1;
        if (ur <= 10 || (ur % 500) == 0) {
            const int peers = playout_->peer_count();
            if (peers > 0) {
                MELLO_LOG_INFO("pipeline", "mix UNDERRUN #%u: peers=%d (no peer produced audio)",
                               ur, peers);
            }
        }
    } else {
        static uint32_t mix_log_ctr = 0;
        if ((mix_log_ctr++ % 1000) == 0) {
            double sum = 0.0;
            for (size_t i = 0; i < kFrames; ++i) {
                const double v = mono_[i] / 32768.0;
                sum += v * v;
            }
            MELLO_LOG_INFO("pipeline", "mix #%u: rms=%.4f sources=%d late_frames=%llu",
                           mix_log_ctr, std::sqrt(sum / kFrames), sources,
                           static_cast<unsigned long long>(playout_->late_frames()));
        }
    }

    // Windowed late playout: warn when late audio is sustained now, not
    // from the lifetime counter.
    const uint64_t late = playout_->late_frames();
    if (late > last_late_frames_) {
        const int64_t now_ms = util::steady_now_ms();
        constexpr int64_t kUnderrunWindowMs = 5000;
        if (underrun_window_start_ms_ == 0) underrun_window_start_ms_ = now_ms;
        underrun_window_count_ += static_cast<int>(late - last_late_frames_);
        const int64_t elapsed = now_ms - underrun_window_start_ms_;
        if (elapsed >= kUnderrunWindowMs) {
            // About 5 late frames per second over the window: audibly degraded.
            if (underrun_window_count_ >= 25 &&
                now_ms - last_underrun_warn_ms_ >= kUnderrunWindowMs) {
                MELLO_LOG_WARN("pipeline",
                               "sustained late playout: %d late frames in last %lldms (%.1f/s) -- output likely degraded",
                               underrun_window_count_, (long long)elapsed,
                               underrun_window_count_ * 1000.0 / elapsed);
                last_underrun_warn_ms_ = now_ms;
            }
            underrun_window_start_ms_ = now_ms;
            underrun_window_count_ = 0;
        }
    }
    last_late_frames_ = late;

    // Clip buffer: remote playback plus the local mic ("all participants").
    if (clip_buffer_ && clip_buffer_->is_active()) {
        if (local_clip_ring_ && local_clip_ring_->available() >= kFrames) {
            std::array<int16_t, kFrames> local{};
            local_clip_ring_->read(local.data(), kFrames);
            std::array<int16_t, kFrames> clip_mix{};
            for (size_t i = 0; i < kFrames; ++i) {
                const int32_t mixed = static_cast<int32_t>(mono_[i]) + static_cast<int32_t>(local[i]);
                clip_mix[i] = static_cast<int16_t>(std::clamp(mixed, -32768, 32767));
            }
            clip_buffer_->write(clip_mix.data(), kFrames);
        } else {
            clip_buffer_->write(mono_.data(), kFrames);
        }
    }

    // Clip playback into the speaker output (retained PCM, lock-free read).
    bool has_clip_audio = false;
    if (clip_playing_.load(std::memory_order_relaxed) &&
        !clip_paused_.load(std::memory_order_relaxed)) {
        const size_t pos = clip_playback_pos_.load(std::memory_order_relaxed);
        const size_t total = clip_playback_total_;
        const size_t avail = (pos < total) ? (total - pos) : 0;
        const size_t to_read = (std::min)(kFrames, avail);
        if (to_read > 0) {
            has_clip_audio = true;
            for (size_t i = 0; i < to_read; ++i) {
                const int32_t c = clip_playback_pcm_[pos + i];
                for (size_t ch = 0; ch < 2; ++ch) {
                    const int32_t mixed = static_cast<int32_t>(stereo[2 * i + ch]) + c;
                    stereo[2 * i + ch] = static_cast<int16_t>(std::clamp(mixed, -32768, 32767));
                }
                mono_[i] = static_cast<int16_t>(
                    std::clamp(static_cast<int32_t>(mono_[i]) + c, -32768, 32767));
            }
            clip_playback_pos_.store(pos + to_read, std::memory_order_relaxed);
        }
        if (pos + to_read >= total) {
            clip_playing_.store(false, std::memory_order_relaxed);
            MELLO_LOG_INFO("pipeline", "clip playback finished");
        }
    }

    // Far-end reference: the mixer output that plays, after NetEQ, so a
    // silent peer feeds silence (comfort noise at most), not PLC noise.
    if (remote || has_clip_audio) {
        echo_canceller_.process_render(mono_.data(), static_cast<int>(kFrames));
        if (echo_suppressor_.enabled()) {
            echo_suppressor_.feed_far_end(mono_.data(), kFrames);
        }
    }

    ring_audible_ = remote || has_clip_audio;
    ring_pos_ = 0;
}

size_t AudioPipeline::mix_output(int16_t* out, size_t count, uint32_t channels) {
    constexpr size_t kFrames = VoicePlayout::kFrameSamples;
    if (channels == 0) channels = 1;
    const size_t frames = count / channels;
    bool audible = false;
    size_t done = 0;
    while (done < frames) {
        if (ring_pos_ >= kFrames) render_frame();
        const size_t n = (std::min)(kFrames - ring_pos_, frames - done);
        const int16_t* src = ring_.data() + ring_pos_ * 2;
        int16_t* dst = out + done * channels;
        if (channels == 1) {
            for (size_t i = 0; i < n; ++i) {
                dst[i] = static_cast<int16_t>(
                    (static_cast<int32_t>(src[2 * i]) + static_cast<int32_t>(src[2 * i + 1])) / 2);
            }
        } else {
            for (size_t i = 0; i < n; ++i) {
                dst[i * channels] = src[2 * i];
                dst[i * channels + 1] = src[2 * i + 1];
                for (uint32_t c = 2; c < channels; ++c) dst[i * channels + c] = 0;
            }
        }
        audible = audible || ring_audible_;
        ring_pos_ += n;
        done += n;
    }
    // A count that is not whole frames: the rest is silence.
    for (size_t i = frames * channels; i < count; ++i) out[i] = 0;
    ring_frames_left_.store(static_cast<int>(kFrames - ring_pos_), std::memory_order_relaxed);
    return audible ? count : 0;
}

int AudioPipeline::render_test_output(int16_t* out, size_t count) {
    auto* test_playback = dynamic_cast<TestPlayback*>(playback_.get());
    if (!test_playback) return -1;
    return static_cast<int>(test_playback->render(out, count));
}

ReceiveStats AudioPipeline::receive_stats() const {
    ReceiveStats s;
    s.playout = playout_->stats();
    s.output_ring_ms =
        static_cast<float>(ring_frames_left_.load(std::memory_order_relaxed)) / 48.0f;
    return s;
}

float AudioPipeline::pipeline_delay_ms() const {
    // NetEQ's filtered buffer delay (mean over peers) plus the output ring.
    // Not the playout timestamp: NetEQ reports none during comfort noise.
    const PlayoutStats p = playout_->stats();
    const float ring_ms =
        static_cast<float>(ring_frames_left_.load(std::memory_order_relaxed)) / 48.0f;
    return p.peers > 0 ? p.filtered_delay_ms + ring_ms : 0.0f;
}

void AudioPipeline::refresh_stream_delay_hint() {
    if (!capture_ || !playback_) return;
    // Render-to-capture latency = output ring + output device latency +
    // input device latency. The render reference is the mixer output
    // (render_frame), so NetEQ's buffer is before it and not in the echo
    // path. Sum, not difference: both device legs add delay. WASAPI
    // latencies come from GetStreamLatency + GetDevicePeriod. The AEC delay
    // estimator converges the rest.
    const int out_ms = playback_->output_latency_ms();
    const int in_ms = capture_->input_latency_ms();
    const float ring_ms =
        static_cast<float>(ring_frames_left_.load(std::memory_order_relaxed)) / 48.0f;
    int delay = out_ms + in_ms + static_cast<int>(ring_ms + 0.5f);
    if (delay < 0) delay = 0;
    if (delay > 500) delay = 500;
    echo_canceller_.set_stream_delay_ms(delay);
    MELLO_LOG_INFO("pipeline", "stream delay hint: out=%d in=%d ring=%.1f total=%d ms",
                   out_ms, in_ms, ring_ms, delay);
}

void AudioPipeline::clear_remote_streams() {
    const size_t had = playout_->clear();
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

#ifdef MELLO_HAS_VPIO_DUPLEX
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

#ifdef MELLO_HAS_VPIO_DUPLEX
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
    request_stereo_output(playback_.get());
    try { init_ok = playback_->initialize(device_id); } catch (...) { init_ok = false; }
    if (!init_ok) {
        MELLO_LOG_WARN("pipeline", "playback device switch failed, falling back to default");
        playback_ = create_audio_playback();
#ifdef _WIN32
        apply_session(playback_.get());
#endif
        request_stereo_output(playback_.get());
        if (!playback_->initialize(nullptr)) {
            MELLO_LOG_ERROR("pipeline", "default playback device also failed");
            return 0;
        }
        fell_back = true;
    }
    wire_render_source();
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
    // The test backend (MELLO_AUDIO_BACKEND=test) is not a WasapiPlayback.
    auto* wasapi = dynamic_cast<WasapiPlayback*>(pb);
    if (!wasapi) return;
    wasapi->set_session(session_win_.get());
}
#endif

} // namespace mello::audio
