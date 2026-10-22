#pragma once
#include "mello.h"
#include "audio_platform.hpp"
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "opus_codec.hpp"
#include "send_adaptation.hpp"
#include "noise_suppressor.hpp"
#include "echo_canceller.hpp"
#include "echo_suppressor.hpp"
#include "speech_expander.hpp"
#include "voice_playout.hpp"
#include "device_enumerator.hpp"
#include "clip_buffer.hpp"
#include "clip_encoder.hpp"
#include "vad.hpp"
#include "../util/ring_buffer.hpp"
#ifdef _WIN32
#include "audio_session_win.hpp"
#endif
#include <mutex>
#include <vector>
#include <queue>
#include <deque>
#include <array>
#include <atomic>
#include <cstdint>
#include <functional>
#include <unordered_map>
#include <string>
#include <memory>

namespace mello::audio {

struct EncodedPacket {
    std::vector<uint8_t> data;
    uint32_t sequence;
    // Media time of the frame's first sample: the 48 kHz capture sample
    // index (see AudioPipeline::capture_timestamp_). The RTP timestamp.
    uint32_t timestamp;
};

/// Size of the voice frame header that mello_voice_feed_packet takes (spec
/// 10 section 4.2): the 32-bit sender sequence and the 32-bit RTP timestamp,
/// both little-endian, then the Opus payload.
static constexpr int VOICE_FRAME_HEADER_SIZE = 8;

/// Receive-path statistics for mello_get_debug_stats (spec 10 section 5.4).
/// Send-leg feedback and the encoder adaptation, for mello_get_debug_stats
/// (spec 10 sections 4.6 and 7.5).
struct SendStats {
    /// The last receiver report about the send track in this session.
    MelloSendFeedback feedback{};
    /// Reports in this session; 0: `feedback` is empty.
    uint32_t reports = 0;
    int bitrate_kbps = 0;
    int max_bitrate_kbps = 0;
    int loss_perc = 0;
    uint32_t rung_changes = 0;
    /// Loss over the step-down window, percent; -1 when unknown.
    float window_loss_pct = -1.0f;
};

struct ReceiveStats {
    /// NetEQ and mixer, over every remote peer.
    PlayoutStats playout;
    /// Mixed audio that waits in the output ring for the device callback.
    float output_ring_ms = 0.0f;
};

enum class NsMode {
    Off = 0,
    Rnnoise = 1,
    WebRtcLow = 2,
    WebRtcModerate = 3,
    WebRtcHigh = 4,
    WebRtcVeryHigh = 5,
};

class AudioPipeline {
public:
    AudioPipeline();
    ~AudioPipeline();

    bool initialize();
    void shutdown();

    bool start_capture();
    void stop_capture();
    bool start_capture_inject();
    void stop_capture_inject();
    void inject_capture(const int16_t* samples, int count);

    void set_mute(bool muted);
    void set_deafen(bool deafened);
    void set_push_to_talk(bool enabled);
    /// Input sensitivity (spec 10 section 4.1). Auto (the default): the RMS
    /// speech gate tracks the ambient noise floor. Manual: the gate opens
    /// when the raw microphone frame RMS reaches `db` dBFS (clamped to
    /// -100..0). Silero VAD confirms speech at VAD_THRESHOLD in both modes.
    /// Any thread.
    void set_input_sensitivity(float db);
    void set_input_sensitivity_auto(bool enabled);
    bool input_sensitivity_auto() const { return sensitivity_auto_.load(std::memory_order_relaxed); }
    float input_sensitivity_db() const { return sensitivity_db_.load(std::memory_order_relaxed); }
    /// Gate threshold (dBFS) that the last captured frame was compared with.
    float input_gate_dbfs() const;
    /// Frames that passed the RMS gate (candidate speech), lifetime.
    uint32_t gate_candidate_frames() const { return gate_candidate_frames_.load(std::memory_order_relaxed); }
    /// Speech gate pre-roll (spec 10 section 4.1): the frames captured
    /// before the gate opens that leave with the first speech frame.
    /// Clamped to 0..kMaxPreRollFrames. The default comes from the voice
    /// gate sweep; the test backend may override it at construction
    /// (MELLO_VOICE_PRE_ROLL_FRAMES). Any thread; takes the capture lock.
    static constexpr int kMaxPreRollFrames = 50;
    void set_speech_pre_roll_frames(int frames);
    int speech_pre_roll_frames() const { return pre_roll_frames_.load(std::memory_order_relaxed); }
    /// Test seam: called on the capture thread with each 20 ms frame right
    /// before Opus encode (after noise suppression), with its media time.
    /// Tests compare it with the injected audio. Takes the capture lock.
    using EncodeTap = std::function<void(const int16_t* pcm, int samples, uint32_t timestamp)>;
    void set_encode_tap(EncodeTap tap);
    void set_input_volume(float vol) { input_gain_.store(vol, std::memory_order_relaxed); }
    void set_output_volume(float vol);
    float input_volume() const { return input_gain_.load(std::memory_order_relaxed); }
    float output_volume() const { return output_gain_.load(std::memory_order_relaxed); }
    void set_echo_cancellation(bool enabled);
    void set_agc(bool enabled) { echo_canceller_.set_agc_enabled(enabled); }
    /// Neural residual-echo suppressor (flag-off rollout). Soft dependency:
    /// missing model degrades to passthrough, never blocks audio.
    void set_echo_suppression(bool enabled) { echo_suppressor_.set_enabled(enabled); }
    bool echo_suppression_enabled() const { return echo_suppressor_.enabled(); }
    void set_noise_suppression(bool enabled) { set_ns_mode(enabled ? NsMode::Rnnoise : NsMode::Off); }
    void set_ns_mode(NsMode mode);
    NsMode ns_mode() const { return static_cast<NsMode>(ns_mode_.load(std::memory_order_relaxed)); }
    void set_transient_suppression(bool enabled);
    void set_high_pass_filter(bool enabled);
    bool echo_cancellation_enabled() const { return echo_canceller_.aec_enabled(); }
    bool agc_enabled() const { return echo_canceller_.agc_enabled(); }
    bool noise_suppression_enabled() const { return ns_mode() != NsMode::Off; }
    bool transient_suppression_enabled() const {
        return transient_suppression_enabled_.load(std::memory_order_relaxed);
    }
    bool high_pass_filter_enabled() const {
        return high_pass_filter_enabled_.load(std::memory_order_relaxed);
    }
    uint32_t aec_capture_frames() const { return echo_canceller_.capture_frames(); }
    uint32_t aec_render_frames() const { return echo_canceller_.render_frames(); }
    bool is_muted() const { return muted_; }
    bool is_deafened() const { return deafened_; }

    int get_packet(uint8_t* buffer, int buffer_size, uint32_t* out_timestamp = nullptr);

    /// One receiver report about the voice send track (spec 10 section
    /// 4.6). Runs the adaptation controller at util::steady_now_ms() and
    /// hands the new encoder bitrate and loss percentage to the capture
    /// thread through atomics. Any thread; in production the
    /// libdatachannel thread. Never takes the capture lock.
    void report_send_feedback(const MelloSendFeedback& feedback);
    /// The channel bitrate in kbps: the top rung of the ladder (24..96).
    /// Any thread.
    void set_max_bitrate_kbps(int kbps);
    SendStats send_stats() const;

    /// Receives each encoded voice frame on the capture thread, right after
    /// Opus encode: the Opus payload (no sequence header) and its size, the
    /// media time (48 kHz, as get_packet reports it) and the sequence.
    using PacketSink = std::function<void(const uint8_t* data, int size,
                                          uint32_t timestamp, uint32_t sequence)>;
    /// Set the packet sink (spec 10 section 4.5). While a sink is set, an
    /// encoded frame goes to the sink and not to the get_packet queue. An
    /// empty function clears the sink. Set and clear wait for a sink call
    /// that runs now, so the caller may release the sink's state after a
    /// clear returns. Setting a sink drops the frames in the queue. Any
    /// thread, but never from inside the sink (it would wait for itself).
    void set_packet_sink(PacketSink sink);

    /// Receive one voice frame of the loopback path (spec 10 section
    /// 4.2): VOICE_FRAME_HEADER_SIZE bytes of header, then Opus.
    /// Builds the RTP fields from the header (payload type 111, the low 16
    /// bits of the sequence, the timestamp, SSRC 0) and inserts the frame
    /// into the peer's NetEQ. Any thread.
    void feed_packet(const char* peer_id, const uint8_t* data, int size);
    /// Receive one complete RTP packet (the SFU path). `receive_time_ms` < 0:
    /// stamp it with util::steady_now_ms(). Any thread.
    void feed_rtp(const char* peer_id, const uint8_t* packet, int size, int64_t receive_time_ms);
    /// Destroy one peer's NetEQ (the peer left). Any thread.
    void remove_peer(const char* peer_id);
    /// Per-peer gain 0..2 and pan -1..+1 (spec 10 section 5.3). Kept for a
    /// peer with no stream yet and across leave and rejoin. Any thread.
    void set_peer_volume(const char* peer_id, float gain);
    void set_peer_pan(const char* peer_id, float pan);
    float peer_volume(const char* peer_id) const;
    float peer_pan(const char* peer_id) const;

    bool is_capturing() const { return capturing_; }
    bool is_speaking() const {
        return push_to_talk_mode_.load(std::memory_order_relaxed)
                   ? false
                   : vad_.is_speaking();
    }
    float speech_probability() const {
        return push_to_talk_mode_.load(std::memory_order_relaxed) ? 0.0f
                                                                  : vad_.probability();
    }
    float rnnoise_probability() const { return noise_suppressor_.speech_probability(); }
    float input_level() const { return input_level_.load(std::memory_order_relaxed); }
    uint32_t packets_encoded() const { return sequence_; }
    int active_streams() const { return playout_->peer_count(); }
    int underrun_count() const { return underrun_count_.load(std::memory_order_relaxed); }
    int rtp_recv_total() const { return rtp_recv_total_.load(std::memory_order_relaxed); }
    float pipeline_delay_ms() const;
    ReceiveStats receive_stats() const;

    /// Test backend only (MELLO_AUDIO_BACKEND=test): pull `count` samples of
    /// mixed output through the playback render source, as the device thread
    /// does. Returns the samples the mixer produced, or -1 when the active
    /// playback backend is not the test backend. Caller thread only.
    int render_test_output(int16_t* out, size_t count);

    using VadCallback = std::function<void(bool speaking)>;
    void set_vad_callback(VadCallback cb) { vad_.set_callback(std::move(cb)); }

    AudioDeviceEnumerator& device_enumerator();
    // Returns: 0 = failed, 1 = ok, 2 = fell back to default device
    int set_capture_device(const char* device_id);
    int set_playback_device(const char* device_id);
    /// The chosen output device ("" = default), copied under a lock. The
    /// stream audio playout reads it on its own thread and opens the same
    /// device as voice.
    std::string playback_device_id() const {
        std::lock_guard<std::mutex> lock(device_id_mutex_);
        return playback_device_id_;
    }

    void start_clip_buffer();
    void stop_clip_buffer();
    bool clip_buffer_active() const;
    bool clip_capture(float seconds, const std::string& output_path);
    bool play_clip(const std::string& wav_path);
    bool play_mp4(const std::string& mp4_path);
    void stop_clip_playback();
    bool clip_is_playing() const;
    void clip_playback_progress(uint64_t& position_samples, uint64_t& total_samples, uint32_t& sample_rate) const;
    void clip_pause();
    void clip_resume();
    void clip_seek(uint64_t position_samples);

private:
    void on_captured_audio(const int16_t* samples, size_t count);
    void process_and_encode_frame(int16_t* frame, uint32_t timestamp);
    /// Capture thread, under accum_mutex_: give the encoder the bitrate and
    /// loss percentage that the controller chose, when they changed.
    void apply_send_adaptation();
    /// A new voice session: the top rung and the loss floor.
    void reset_send_adaptation();
    void reset_speech_gate_state();
    void clear_remote_streams();
    /// (Re)build the capture+playback backend pair for the desired
    /// voice-processing state (macOS: VPIO duplex vs plain HAL pair) and
    /// restart what was running. Falls back to the plain pair when the
    /// duplex unit fails to initialize.
    void switch_audio_backend(bool voice_processing);
    const char* current_capture_device_id() const {
        return capture_device_id_.empty() ? nullptr : capture_device_id_.c_str();
    }
    const char* current_playback_device_id() const {
        return playback_device_id_.empty() ? nullptr : playback_device_id_.c_str();
    }
#ifdef MELLO_HAS_VPIO_DUPLEX
    /// Install a live VPIO duplex pair for the stored device ids. Returns
    /// false when the unit fails (caller falls back to the plain pair).
    bool activate_vpio_pair();
    void activate_plain_pair();
#endif
    /// Recompute APM stream-delay hint from device latencies. The render
    /// reference is the mixer output, so NetEQ's buffer is not in the
    /// render-to-capture path; the output ring is (at most 10 ms). Called on
    /// init and device switches (not per-frame: the estimator converges from
    /// a close start).
    void refresh_stream_delay_hint();
#ifdef _WIN32
    void apply_session(AudioPlayback* pb);
#endif

    /// Ask the new playback backend for stereo, before its initialize().
    /// Backends that cannot (VPIO, RemoteIO, the test backend) stay mono
    /// and report it through render_channels().
    void request_stereo_output(AudioPlayback* pb);
    /// Set the playback render source to mix_output with the channel count
    /// of the current backend. Every playback replacement calls it before
    /// start() (spec 03 section 4.2).
    void wire_render_source();
    /// Device-thread render source: `count` interleaved samples of
    /// `channels` channels (1 or 2). Pulls 10 ms frames from render_frame()
    /// through the output ring. Returns `count` when any of it is audible,
    /// else 0 (all silence).
    size_t mix_output(int16_t* out, size_t count, uint32_t channels);
    /// Mix one 10 ms stereo frame into the output ring: NetEQ and the mixer,
    /// deafen, the clip buffer tap, clip playback, and the AEC render
    /// reference. Device thread.
    void render_frame();

    std::unique_ptr<AudioCapture> capture_;
    std::unique_ptr<AudioPlayback> playback_;
#ifdef _WIN32
    std::unique_ptr<AudioSessionWin> session_win_;
#endif
    OpusEnc encoder_;
    // Encoder adaptation (spec 10 section 4.6). The controller and the last
    // report live under adapt_mutex_ (report and control threads; never the
    // capture thread). The capture thread reads the two atomics and keeps
    // what it gave the encoder in applied_* (under accum_mutex_).
    mutable std::mutex adapt_mutex_;
    SendAdaptation adapt_;
    MelloSendFeedback last_feedback_{};
    uint32_t session_reports_ = 0;
    std::atomic<int> adapt_bitrate_bps_{BITRATE};
    std::atomic<int> adapt_loss_perc_{kLossPercFloor};
    int applied_bitrate_bps_ = BITRATE;
    int applied_loss_perc_ = kLossPercFloor;
    NoiseSuppressor noise_suppressor_;
    EchoCanceller echo_canceller_;
    EchoSuppressor echo_suppressor_;
    // Post-AGC downward expander: pushes the AGC-pumped noise floor back down in
    // speech gaps (keyed off the Silero VAD), so a quiet mic that forces large
    // AGC gain does not transmit a swelling floor between words. See
    // plans/AEC-CLIPPING-REPRO.md. Capture-thread only.
    SpeechExpander speech_expander_;
    VoiceActivityDetector vad_;
    std::unique_ptr<AudioDeviceEnumerator> device_enum_;

    // Receive and playout: one NetEQ per peer and the mixer (spec 10
    // section 5). Created in the constructor and never replaced, so the
    // network threads that insert never see it change. It locks itself.
    std::unique_ptr<VoicePlayout> playout_;
    std::atomic<int> underrun_count_{0};
    std::atomic<int> rtp_recv_total_{0};

    // Output ring: the device callback asks for any size, the mixer makes
    // 10 ms stereo frames. Device thread only (render_test_output: the
    // caller thread, with no device thread on the test backend).
    std::array<int16_t, VoicePlayout::kFrameSamples * VoicePlayout::kChannels> ring_{};
    size_t ring_pos_ = VoicePlayout::kFrameSamples;  // frames consumed; 480 = empty
    bool ring_audible_ = false;
    // Frames left in the ring, for pipeline_delay_ms() on other threads.
    std::atomic<int> ring_frames_left_{0};
    // Scratch for render_frame(), device thread only.
    std::array<int16_t, VoicePlayout::kFrameSamples> mono_{};

    // Windowed late-playout health (device thread only; no atomics needed).
    uint64_t last_late_frames_ = 0;
    int underrun_window_count_ = 0;
    int64_t underrun_window_start_ms_ = 0;
    int64_t last_underrun_warn_ms_ = 0;

    std::vector<int16_t> capture_accum_;
    struct PreRollFrame {
        uint32_t timestamp;
        std::array<int16_t, FRAME_SIZE> pcm;
    };
    std::deque<PreRollFrame> speech_pre_roll_;
    // Frames kept before the speech gate opens. Written under accum_mutex_,
    // read by speech_pre_roll_frames() on any thread.
    std::atomic<int> pre_roll_frames_;
    EncodeTap encode_tap_;  // accum_mutex_
    // 48 kHz sample index of the next captured frame. Counts every frame
    // that reaches on_captured_audio, encoded or not, so a gap from the
    // speech gate or mute advances the RTP timestamp like DTX instead of
    // looking like a stall. Guarded by accum_mutex_ (capture thread).
    uint32_t capture_timestamp_ = 0;
    std::mutex accum_mutex_;

    std::queue<EncodedPacket> outgoing_;
    std::mutex outgoing_mutex_;
    // The capture thread holds packet_sink_mutex_ for the whole sink call
    // and for the queue push, so a set or clear waits for a running call
    // and a frame never goes to both. Lock order: accum_mutex_, then
    // packet_sink_mutex_, then outgoing_mutex_.
    PacketSink packet_sink_;
    std::mutex packet_sink_mutex_;
    uint32_t sequence_ = 0;

    std::atomic<bool> muted_{false};
    std::atomic<bool> deafened_{false};
    std::atomic<bool> push_to_talk_mode_{false};
    std::atomic<bool> capturing_{false};
    std::atomic<float> input_level_{0.0f};
    std::atomic<float> input_gain_{1.0f};
    std::atomic<float> output_gain_{1.0f};
    std::atomic<int> ns_mode_{static_cast<int>(NsMode::Rnnoise)};
    std::atomic<bool> transient_suppression_enabled_{false};
    std::atomic<bool> high_pass_filter_enabled_{false};
    std::atomic<bool> capture_inject_mode_{false};
    // Desired capture backend on macOS (echo toggle position). The actual
    // backend is reported by AudioCapture::provides_echo_cancellation().
    std::atomic<bool> voice_processing_capture_{false};
    // Actual backend echo state, cached on the main thread at capture start
    // (the audio thread must not touch the capture_ pointer: device
    // switches can replace it mid-callback).
    std::atomic<bool> backend_cancels_echo_{false};
    // Last requested devices ("empty" = default). Backend and device
    // switches re-open the stored pair.
    std::string capture_device_id_;
    std::string playback_device_id_;
    // Guards the writes of playback_device_id_ and playback_device_id(). The
    // pipeline's own readers run on the writing thread.
    mutable std::mutex device_id_mutex_;
    float noise_floor_rms_ = 0.001f;
    std::atomic<bool> sensitivity_auto_{true};
    std::atomic<float> sensitivity_db_{-40.0f};
    std::atomic<float> gate_threshold_rms_{0.002f};
    std::atomic<uint32_t> gate_candidate_frames_{0};
    int candidate_hangover_frames_ = 0;
    int speech_hangover_frames_ = 0;
    bool speech_gate_active_ = false;
    // Silero ran on the previous frame. When it starts again after the RMS
    // gate kept it idle, its first chunk takes the end of prev_frame_ (the
    // previous frame as captured, before the expander). Capture thread.
    bool vad_fed_last_frame_ = false;
    std::array<int16_t, FRAME_SIZE> prev_frame_{};

    uint32_t get_pkt_ctr_ = 0;

    bool initialized_ = false;

    std::unique_ptr<ClipBuffer> clip_buffer_;
    std::unique_ptr<util::RingBuffer<int16_t>> local_clip_ring_;

    // Clip playback: retained PCM with atomic position for lock-free read from audio thread
    std::vector<int16_t> clip_playback_pcm_;
    std::atomic<size_t> clip_playback_pos_{0};
    size_t clip_playback_total_{0};
    std::atomic<bool> clip_playing_{false};
    std::atomic<bool> clip_paused_{false};
};

} // namespace mello::audio
