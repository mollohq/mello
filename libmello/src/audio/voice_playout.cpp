// Voice playout engine: NetEQ per peer, AudioMixerImpl over them.
// See voice_playout.hpp for the thread and lock rules, and spec 10 section 5.
//
// Compiled in the C++20 object library mello_playout (libmello
// CMakeLists.txt) with WIN32_LEAN_AND_MEAN: the vendored rtc_base headers
// include winsock2.h.

#include "voice_playout.hpp"

#include "../util/log.hpp"
#include "../util/test_clock.hpp"

#include <algorithm>
#include <atomic>
#include <cmath>
#include <mutex>
#include <unordered_map>
#include <utility>

#include "api/array_view.h"
#include "api/audio/audio_frame.h"
#include "api/audio/audio_mixer.h"
#include "api/audio_codecs/audio_decoder_factory_template.h"
#include "api/audio_codecs/audio_format.h"
#include "api/audio_codecs/opus/audio_decoder_opus.h"
#include "api/environment/environment.h"
#include "api/environment/environment_factory.h"
#include "api/field_trials_view.h"
#include "api/neteq/custom_neteq_factory.h"
#include "api/neteq/default_neteq_controller_factory.h"
#include "api/neteq/neteq.h"
#include "api/neteq/neteq_controller.h"
#include "api/neteq/neteq_controller_factory.h"
#include "api/neteq/tick_timer.h"
#include "api/rtp_headers.h"
#include "api/units/timestamp.h"
#include "modules/audio_mixer/audio_mixer_impl.h"
#include "system_wrappers/include/clock.h"
#include "system_wrappers/include/ntp_time.h"

namespace mello::audio {

namespace {

constexpr int kSampleRateHz = 48000;
constexpr uint8_t kCnPayloadType = 13;
constexpr char kDelayManagerTrial[] = "WebRTC-Audio-NetEqDelayManagerConfig";
// A timestamp this far behind the newest one is a new stream (the sender
// restarted its capture clock). Reordered packets are at most a few hundred
// ms behind.
constexpr int32_t kNewStreamBehindSamples = 2 * kSampleRateHz;
// A 10 ms expand frame is late audio when at least one 20 ms packet waits
// in the NetEQ buffer.
constexpr int kLateBufferMs = 20;

// The field trials NetEQ reads: only the delay manager tuning.
class MelloFieldTrials final : public webrtc::FieldTrialsView {
public:
    explicit MelloFieldTrials(std::string delay_manager_config)
        : delay_manager_config_(std::move(delay_manager_config)) {}

    std::string Lookup(absl::string_view key) const override {
        if (key == kDelayManagerTrial) return delay_manager_config_;
        return "";
    }

private:
    std::string delay_manager_config_;
};

// NetEQ's clock: util::steady_now_ms(), the steady clock in production and
// the test clock under the voice quality gate. Never
// Clock::GetRealTimeClock(), which the test clock cannot drive. NetEQ times
// packet arrivals with its own tick timer (one tick per GetAudio); it reads
// this clock for the receive-to-decode delay statistics.
class PlayoutClock final : public webrtc::Clock {
public:
    webrtc::Timestamp CurrentTime() override {
        return webrtc::Timestamp::Millis(util::steady_now_ms());
    }

    webrtc::NtpTime ConvertTimestampToNtpTime(webrtc::Timestamp t) override {
        const int64_t us = t.us() < 0 ? 0 : t.us();
        const uint64_t seconds = static_cast<uint64_t>(us / 1'000'000);
        const uint64_t fractions =
            static_cast<uint64_t>(us % 1'000'000) * webrtc::NtpTime::kFractionsPerSecond / 1'000'000;
        return webrtc::NtpTime(static_cast<uint32_t>(seconds), static_cast<uint32_t>(fractions));
    }
};

// What the talk-spurt controller reports to the engine. Written under the
// NetEQ lock, read by stats().
struct SpurtTelemetry {
    std::atomic<uint64_t> talk_spurts{0};
    std::atomic<int> hold_ms{0};
};

// NetEQ controller for the mello sender (spec 10 section 5.2). It wraps
// NetEQ's own DecisionLogic and changes two things. Both come from voice
// gate measurements: NetEQ's time stretching of speech costs more PESQ MOS
// than the latency it saves in these two cases.
//
// 1. Talk-spurt pre-roll. The mello sender sends nothing while its speech
// gate is closed. When the gate opens, it sends the pre-roll (up to 100 ms
// of audio captured before the gate opened) and the current frame at once.
// So the first packets of a talk spurt arrive up to about 100 ms late
// against their media time, by design, and later packets arrive on time.
// Unchanged, NetEQ learns that lateness as network jitter (target delay 100
// to 180 ms on every gated profile, for seconds) and time-compresses the
// extra audio during the speech (about -0.3 MOS on the clean profile).
// This controller:
// - detects a talk spurt start: the RTP timestamp moves further than the
//   sequence numbers account for (the sender skipped frames), or the first
//   packet;
// - keeps the packets that arrive in the first kStartWindowMs of a spurt
//   (the pre-roll burst, also when network jitter spreads it) out of the
//   arrival statistics;
// - measures the spurt's lateness L in that window: how late the first
//   packet arrived against the schedule of the earliest-scheduled packet.
//   After the window L is fixed, so clock drift in a long spurt cannot move
//   it;
// - holds a minimum delay of L for the spurt, so NetEQ plays the spurt at
//   its natural delay and does not compress it. The next silence removes
//   the extra delay at no cost: the next spurt measures its own L.
//
// 2. Preemptive expand after a target jump. A stall or a jitter spike makes
// the delay manager raise its target in one step. NetEQ then stretches the
// speech (preemptive expand) to fill the buffer up to the new target, and
// shortly after compresses it again (accelerate) when the target decays.
// For Config::jump_no_expand_ms after a target rise of kJumpMs or more this
// controller does not stretch the speech: the buffer stays where it is.
// Real lateness still fills it, through expand. Slow target changes and the
// fill at stream start are NetEQ's own, unchanged.
//
// Network jitter, loss, outages and clock drift are otherwise NetEQ's own
// decisions. A stream without gate gaps (push-to-talk) is one spurt.
//
// Thread: NetEqImpl calls every method under its own lock.
class TalkSpurtController final : public webrtc::NetEqController {
public:
    static constexpr int kMaxHoldMs = 300;
    // The pre-roll and the current frame leave together; network jitter can
    // spread them over a few ticks. The window covers that and is short
    // enough that clock drift cannot add to L.
    static constexpr int64_t kStartWindowMs = 200;
    // The timestamp moved at least this many frames further than the
    // sequence: the sender paused (a gate gap, not jitter or loss).
    static constexpr int kGapFrames = 2;
    // A target rise of at least this much in one decision is a jump.
    static constexpr int kJumpMs = 20;

    // The pre-roll handling on or off; lateness below hold_floor_ms is
    // network jitter, not a pre-roll: no hold; no preemptive expand for
    // jump_no_expand_ms after a jump (VoicePlayout::Config).
    struct Tuning {
        bool spurt_hold = true;
        int hold_floor_ms = 40;
        int64_t jump_no_expand_ms = 5000;
    };

    TalkSpurtController(std::unique_ptr<webrtc::NetEqController> inner,
                        const webrtc::TickTimer* tick_timer,
                        std::shared_ptr<SpurtTelemetry> telemetry, Tuning tuning)
        : inner_(std::move(inner)),
          tick_timer_(tick_timer),
          telemetry_(std::move(telemetry)),
          tuning_(tuning) {}

    std::optional<int> PacketArrived(int fs_hz, bool should_update_stats,
                                     const PacketArrivedInfo& info) override {
        const int fs_khz = std::max(1, fs_hz / 1000);
        const int64_t now_ms = this->now_ms();
        const int64_t frame = info.packet_length_samples > 0
                                  ? static_cast<int64_t>(info.packet_length_samples)
                                  : 20 * fs_khz;
        bool newest = true;
        bool spurt_start = !have_newest_;
        if (have_newest_) {
            const int32_t ts_step = static_cast<int32_t>(info.main_timestamp - newest_timestamp_);
            const int16_t seq_step =
                static_cast<int16_t>(info.main_sequence_number - newest_sequence_);
            newest = ts_step > 0;
            if (newest && seq_step >= 1 &&
                static_cast<int64_t>(ts_step) >
                    (static_cast<int64_t>(seq_step) + kGapFrames) * frame) {
                spurt_start = true;
            }
        }
        if (newest) {
            have_newest_ = true;
            newest_timestamp_ = info.main_timestamp;
            newest_sequence_ = info.main_sequence_number;
        }
        if (spurt_start) {
            spurt_first_arrival_ms_ = now_ms;
            spurt_first_timestamp_ = info.main_timestamp;
            spurt_min_rel_ms_ = 0;
            telemetry_->talk_spurts.fetch_add(1, std::memory_order_relaxed);
        }

        const bool in_window =
            tuning_.spurt_hold && now_ms - spurt_first_arrival_ms_ < kStartWindowMs;
        if (in_window) {
            // Arrival against the schedule of the spurt's first packet.
            // Negative: earlier than that schedule, so the first was late.
            const int64_t media_ms =
                static_cast<int32_t>(info.main_timestamp - spurt_first_timestamp_) / fs_khz;
            const int64_t rel_ms = (now_ms - spurt_first_arrival_ms_) - media_ms;
            spurt_min_rel_ms_ = std::min(spurt_min_rel_ms_, rel_ms);
        }

        const auto result =
            inner_->PacketArrived(fs_hz, should_update_stats && !in_window, info);
        if (!tuning_.spurt_hold) return result;

        // The hold follows the spurt's lateness as the pre-roll arrives: 0 at
        // the spurt's first packet, then up to L. The pre-roll arrives before
        // NetEQ's next decision (or within a few ticks under jitter), and
        // NetEQ does not time-stretch right after a silence, so nothing is
        // compressed meanwhile. A spurt never inherits the previous spurt's
        // hold.
        const int lateness =
            static_cast<int>(std::min<int64_t>(-spurt_min_rel_ms_, kMaxHoldMs));
        const int hold = lateness >= tuning_.hold_floor_ms ? lateness : 0;
        if (hold != hold_ms_ && inner_->SetMinimumDelay(hold)) {
            hold_ms_ = hold;
            telemetry_->hold_ms.store(hold, std::memory_order_relaxed);
        }
        return result;
    }

    webrtc::NetEq::Operation GetDecision(const NetEqStatus& status, bool* reset_decoder) override {
        webrtc::NetEq::Operation op = inner_->GetDecision(status, reset_decoder);
        const int64_t now = now_ms();
        const int target = inner_->TargetLevelMs();
        if (last_target_ms_ >= 0 && target >= last_target_ms_ + kJumpMs) {
            last_jump_ms_ = now;
        }
        last_target_ms_ = target;
        if (op == webrtc::NetEq::Operation::kPreemptiveExpand && last_jump_ms_ >= 0 &&
            now - last_jump_ms_ < tuning_.jump_no_expand_ms) {
            op = webrtc::NetEq::Operation::kNormal;
        }
        return op;
    }

    void Reset() override {
        inner_->Reset();
        have_newest_ = false;
        hold_ms_ = 0;
        last_target_ms_ = -1;
        last_jump_ms_ = -1;
        telemetry_->hold_ms.store(0, std::memory_order_relaxed);
    }

    // Everything else is NetEQ's DecisionLogic.
    void SoftReset() override { inner_->SoftReset(); }
    void RegisterEmptyPacket() override { inner_->RegisterEmptyPacket(); }
    void SetSampleRate(int fs_hz, size_t output_size_samples) override {
        inner_->SetSampleRate(fs_hz, output_size_samples);
    }
    bool SetMaximumDelay(int delay_ms) override { return inner_->SetMaximumDelay(delay_ms); }
    bool SetMinimumDelay(int delay_ms) override { return inner_->SetMinimumDelay(delay_ms); }
    bool SetBaseMinimumDelay(int delay_ms) override { return inner_->SetBaseMinimumDelay(delay_ms); }
    int GetBaseMinimumDelay() const override { return inner_->GetBaseMinimumDelay(); }
    void ExpandDecision(webrtc::NetEq::Operation operation) override {
        inner_->ExpandDecision(operation);
    }
    void AddSampleMemory(int32_t value) override { inner_->AddSampleMemory(value); }
    int TargetLevelMs() const override { return inner_->TargetLevelMs(); }
    int UnlimitedTargetLevelMs() const override { return inner_->UnlimitedTargetLevelMs(); }
    void NotifyMutedState() override { inner_->NotifyMutedState(); }
    bool PeakFound() const override { return inner_->PeakFound(); }
    int GetFilteredBufferLevel() const override { return inner_->GetFilteredBufferLevel(); }
    void set_sample_memory(int32_t value) override { inner_->set_sample_memory(value); }
    size_t noise_fast_forward() const override { return inner_->noise_fast_forward(); }
    size_t packet_length_samples() const override { return inner_->packet_length_samples(); }
    void set_packet_length_samples(size_t value) override {
        inner_->set_packet_length_samples(value);
    }
    void set_prev_time_scale(bool value) override { inner_->set_prev_time_scale(value); }

private:
    // NetEQ's own time: one tick per GetAudio.
    int64_t now_ms() const {
        return static_cast<int64_t>(tick_timer_->ticks()) * tick_timer_->ms_per_tick();
    }

    std::unique_ptr<webrtc::NetEqController> inner_;
    const webrtc::TickTimer* tick_timer_;
    std::shared_ptr<SpurtTelemetry> telemetry_;
    const Tuning tuning_;
    bool have_newest_ = false;
    uint32_t newest_timestamp_ = 0;
    uint16_t newest_sequence_ = 0;
    int64_t spurt_first_arrival_ms_ = 0;
    uint32_t spurt_first_timestamp_ = 0;
    int64_t spurt_min_rel_ms_ = 0;
    int hold_ms_ = 0;
    int last_target_ms_ = -1;
    int64_t last_jump_ms_ = -1;
};

class TalkSpurtControllerFactory final : public webrtc::NetEqControllerFactory {
public:
    TalkSpurtControllerFactory(std::shared_ptr<SpurtTelemetry> telemetry,
                               TalkSpurtController::Tuning tuning)
        : telemetry_(std::move(telemetry)), tuning_(tuning) {}

    std::unique_ptr<webrtc::NetEqController> Create(
        const webrtc::Environment& env, const webrtc::NetEqController::Config& config) const override {
        return std::make_unique<TalkSpurtController>(
            webrtc::DefaultNetEqControllerFactory().Create(env, config), config.tick_timer,
            telemetry_, tuning_);
    }

private:
    std::shared_ptr<SpurtTelemetry> telemetry_;
    TalkSpurtController::Tuning tuning_;
};

int16_t scale_sample(int16_t s, float gain) {
    const float v = static_cast<float>(s) * gain;
    if (v >= 32767.0f) return 32767;
    if (v <= -32768.0f) return -32768;
    return static_cast<int16_t>(std::lrintf(v));
}

}  // namespace

bool parse_rtp(const uint8_t* packet, size_t size, RtpInfo* info, size_t* payload_offset,
               size_t* payload_size) {
    if (!packet || size < 12) return false;
    if ((packet[0] >> 6) != 2) return false;  // RTP version 2
    const bool padding = (packet[0] & 0x20) != 0;
    const bool extension = (packet[0] & 0x10) != 0;
    const size_t csrc_count = packet[0] & 0x0F;
    size_t header = 12 + csrc_count * 4;
    if (header > size) return false;
    if (extension) {
        if (header + 4 > size) return false;
        const size_t words = (static_cast<size_t>(packet[header + 2]) << 8) | packet[header + 3];
        header += 4 + words * 4;
        if (header > size) return false;
    }
    size_t end = size;
    if (padding) {
        const size_t pad = packet[size - 1];
        if (pad == 0 || header + pad > size) return false;
        end -= pad;
    }
    if (end <= header) return false;  // no payload
    if (info) {
        info->payload_type = packet[1] & 0x7F;
        info->sequence = static_cast<uint16_t>((packet[2] << 8) | packet[3]);
        info->timestamp = (static_cast<uint32_t>(packet[4]) << 24) |
                          (static_cast<uint32_t>(packet[5]) << 16) |
                          (static_cast<uint32_t>(packet[6]) << 8) | packet[7];
        info->ssrc = (static_cast<uint32_t>(packet[8]) << 24) |
                     (static_cast<uint32_t>(packet[9]) << 16) |
                     (static_cast<uint32_t>(packet[10]) << 8) | packet[11];
    }
    if (payload_offset) *payload_offset = header;
    if (payload_size) *payload_size = end - header;
    return true;
}

namespace {

// Counters that the device thread and the network threads write.
struct EngineCounters {
    std::atomic<uint64_t> packets_inserted{0};
    std::atomic<uint64_t> insert_errors{0};
    std::atomic<uint64_t> get_audio_errors{0};
    std::atomic<uint64_t> packets_rejected{0};
    std::atomic<uint64_t> stream_resets{0};
    std::atomic<uint64_t> late_frames{0};
    std::atomic<float> output_gain{1.0f};
    // Device thread only: sources that produced audio in this mix.
    int contributing = 0;
};

// One remote peer: its NetEQ and its mixer source. The mixer calls
// GetAudioFrameWithInfo on the device thread; insert() calls the NetEQ on a
// network thread. NetEqImpl locks itself, so the two may overlap.
class PeerStream final : public webrtc::AudioMixer::Source {
public:
    PeerStream(EngineCounters& counters, std::unique_ptr<webrtc::NetEq> neteq,
               std::shared_ptr<SpurtTelemetry> spurts, uint32_t ssrc, int id, float volume,
               float pan)
        : ssrc(ssrc), volume(volume), pan(pan), spurts(std::move(spurts)), counters_(counters),
          neteq_(std::move(neteq)), id_(id) {}

    AudioFrameInfo GetAudioFrameWithInfo(int sample_rate_hz, webrtc::AudioFrame* frame) override {
        bool muted = false;
        if (neteq_->GetAudio(&decoded_, &muted) != webrtc::NetEq::kOK) {
            counters_.get_audio_errors.fetch_add(1, std::memory_order_relaxed);
            return AudioFrameInfo::kError;
        }
        const auto type = decoded_.speech_type_;
        if ((type == webrtc::AudioFrame::kPLC || type == webrtc::AudioFrame::kCodecPLC) &&
            neteq_->CurrentNetworkStatistics().current_buffer_size_ms >= kLateBufferMs) {
            counters_.late_frames.fetch_add(1, std::memory_order_relaxed);
        }
        if (muted) return AudioFrameInfo::kMuted;

        const size_t n = decoded_.samples_per_channel_;
        if (decoded_.sample_rate_hz_ != sample_rate_hz || decoded_.num_channels_ != 1 || n == 0 ||
            n > webrtc::AudioFrame::kMaxDataSizeSamples / 2) {
            if (!format_warned_) {
                format_warned_ = true;
                MELLO_LOG_WARN("playout", "unexpected NetEQ frame: rate=%d ch=%zu n=%zu (mixer rate %d)",
                               decoded_.sample_rate_hz_, decoded_.num_channels_, n, sample_rate_hz);
            }
            return AudioFrameInfo::kError;
        }

        // Volume and pan before the mixer's limiter. Balance pan: the far
        // channel fades, the near one stays at unity, so centre is unity on
        // both and a mono downmix equals the decoded signal.
        const float gain = volume.load(std::memory_order_relaxed) *
                           counters_.output_gain.load(std::memory_order_relaxed);
        const float p = pan.load(std::memory_order_relaxed);
        const float gain_l = gain * std::min(1.0f, 1.0f - p);
        const float gain_r = gain * std::min(1.0f, 1.0f + p);

        frame->UpdateFrame(decoded_.timestamp_, nullptr, n, sample_rate_hz, type,
                           decoded_.vad_activity_, 2);
        int16_t* dst = frame->mutable_data();
        const int16_t* src = decoded_.data();
        for (size_t i = 0; i < n; ++i) {
            dst[2 * i] = scale_sample(src[i], gain_l);
            dst[2 * i + 1] = scale_sample(src[i], gain_r);
        }
        ++counters_.contributing;
        return AudioFrameInfo::kNormal;
    }

    int Ssrc() const override { return id_; }
    int PreferredSampleRate() const override { return kSampleRateHz; }

    webrtc::NetEq& neteq() { return *neteq_; }

    // Insert-side state, guarded by the engine's peer mutex.
    uint32_t ssrc = 0;
    bool has_last_timestamp = false;
    uint32_t last_timestamp = 0;

    std::atomic<float> volume{1.0f};
    std::atomic<float> pan{0.0f};
    // Written by this stream's TalkSpurtController.
    const std::shared_ptr<SpurtTelemetry> spurts;

private:
    EngineCounters& counters_;
    std::unique_ptr<webrtc::NetEq> neteq_;
    webrtc::AudioFrame decoded_;  // device thread only
    bool format_warned_ = false;  // device thread only
    int id_;
};

void add_lifetime(PlayoutStats& s, const webrtc::NetEqLifetimeStatistics& lt,
                  const webrtc::NetEqOperationsAndState& ops) {
    s.total_samples_received += lt.total_samples_received;
    s.concealed_samples += lt.concealed_samples;
    s.silent_concealed_samples += lt.silent_concealed_samples;
    s.concealment_events += lt.concealment_events;
    s.inserted_samples_for_deceleration += lt.inserted_samples_for_deceleration;
    s.removed_samples_for_acceleration += lt.removed_samples_for_acceleration;
    s.fec_packets_received += lt.fec_packets_received;
    s.fec_packets_discarded += lt.fec_packets_discarded;
    s.packets_discarded += lt.packets_discarded;
    s.jitter_buffer_delay_ms += lt.jitter_buffer_delay_ms;
    s.jitter_buffer_emitted_count += lt.jitter_buffer_emitted_count;
    s.interruption_count += static_cast<uint64_t>(std::max(0, lt.interruption_count));
    s.total_interruption_duration_ms +=
        static_cast<uint64_t>(std::max(0, lt.total_interruption_duration_ms));
    // NetEQ flushes its empty packet buffer when the first packet arrives;
    // that one is a start, not a reset.
    s.packet_buffer_flushes += ops.packet_buffer_flushes > 0 ? ops.packet_buffer_flushes - 1 : 0;
}

}  // namespace

struct VoicePlayout::Impl {
    explicit Impl(Config cfg)
        : config(std::move(cfg)),
          env(webrtc::CreateEnvironment(
              std::make_unique<MelloFieldTrials>(config.delay_manager_config),
              std::make_unique<PlayoutClock>())),
          decoder_factory(webrtc::CreateAudioDecoderFactory<webrtc::AudioDecoderOpus>()),
          mixer(webrtc::AudioMixerImpl::Create()) {}

    std::unique_ptr<webrtc::NetEq> make_neteq(const std::shared_ptr<SpurtTelemetry>& spurts) {
        webrtc::NetEq::Config c;
        c.sample_rate_hz = kSampleRateHz;
        c.enable_muted_state = config.enable_muted_state;
        c.enable_fast_accelerate = config.enable_fast_accelerate;
        // NetEQ with its own DecisionLogic inside the talk-spurt controller.
        auto neteq =
            webrtc::CustomNetEqFactory(
                std::make_unique<TalkSpurtControllerFactory>(
                    spurts, TalkSpurtController::Tuning{config.talk_spurt_hold,
                                                        config.spurt_hold_floor_ms,
                                                        config.jump_no_expand_ms}))
                .Create(env, c, decoder_factory);
        if (!neteq->RegisterPayloadType(kOpusPayloadType,
                                        webrtc::SdpAudioFormat("opus", kSampleRateHz, 2,
                                                               {{"stereo", "0"}})) ||
            !neteq->RegisterPayloadType(kCnPayloadType,
                                        webrtc::SdpAudioFormat("CN", kSampleRateHz, 1))) {
            MELLO_LOG_ERROR("playout", "NetEQ payload type registration failed");
        }
        return neteq;
    }

    // Takes the stream out of the mixer and keeps its lifetime counts.
    // Caller holds mutex. RemoveSource waits for a Mix that runs now.
    void retire_locked(const std::shared_ptr<PeerStream>& stream) {
        mixer->RemoveSource(stream.get());
        add_lifetime(retired, stream->neteq().GetLifetimeStatistics(),
                     stream->neteq().GetOperationsAndState());
        retired.talk_spurts += stream->spurts->talk_spurts.load(std::memory_order_relaxed);
    }

    float setting_locked(const std::unordered_map<std::string, float>& map, const std::string& peer,
                         float def) const {
        auto it = map.find(peer);
        return it == map.end() ? def : it->second;
    }

    Config config;
    webrtc::Environment env;
    rtc::scoped_refptr<webrtc::AudioDecoderFactory> decoder_factory;
    rtc::scoped_refptr<webrtc::AudioMixerImpl> mixer;
    EngineCounters counters;

    mutable std::mutex mutex;  // peers, volumes, pans, retired, next_id
    std::unordered_map<std::string, std::shared_ptr<PeerStream>> peers;
    std::unordered_map<std::string, float> volumes;
    std::unordered_map<std::string, float> pans;
    PlayoutStats retired;  // lifetime fields only
    int next_id = 1;

    webrtc::AudioFrame mixed;  // device thread only
};

const char* VoicePlayout::default_delay_manager_config() {
    return "forget_factor:0.6,reorder_forget_factor:0.98";
}

VoicePlayout::VoicePlayout(Config config) : impl_(std::make_unique<Impl>(std::move(config))) {
    MELLO_LOG_INFO("playout", "NetEQ playout ready (delay manager '%s', muted state %s)",
                   impl_->config.delay_manager_config.c_str(),
                   impl_->config.enable_muted_state ? "on" : "off");
}

VoicePlayout::~VoicePlayout() { clear(); }

bool VoicePlayout::insert(const std::string& peer, const RtpInfo& rtp, const uint8_t* payload,
                          size_t payload_size, int64_t receive_time_ms) {
    auto& c = impl_->counters;
    if ((rtp.payload_type != kOpusPayloadType && rtp.payload_type != kCnPayloadType) || !payload ||
        payload_size == 0) {
        c.packets_rejected.fetch_add(1, std::memory_order_relaxed);
        return false;
    }

    std::shared_ptr<PeerStream> stream;
    {
        std::lock_guard<std::mutex> lock(impl_->mutex);
        auto it = impl_->peers.find(peer);
        if (it != impl_->peers.end()) {
            PeerStream& s = *it->second;
            const char* why = nullptr;
            if (s.ssrc != rtp.ssrc) {
                why = "new SSRC";
            } else if (s.has_last_timestamp &&
                       static_cast<int32_t>(rtp.timestamp - s.last_timestamp) <
                           -kNewStreamBehindSamples) {
                why = "timestamp far behind (sender restart)";
            }
            if (why) {
                MELLO_LOG_INFO("playout", "peer '%s': new stream (%s): ssrc %08x -> %08x, ts %u -> %u",
                               peer.c_str(), why, s.ssrc, rtp.ssrc, s.last_timestamp, rtp.timestamp);
                impl_->retire_locked(it->second);
                impl_->peers.erase(it);
                it = impl_->peers.end();
                c.stream_resets.fetch_add(1, std::memory_order_relaxed);
            }
        }
        if (it == impl_->peers.end()) {
            auto spurts = std::make_shared<SpurtTelemetry>();
            auto created = std::make_shared<PeerStream>(
                c, impl_->make_neteq(spurts), spurts, rtp.ssrc, impl_->next_id++,
                impl_->setting_locked(impl_->volumes, peer, 1.0f),
                impl_->setting_locked(impl_->pans, peer, 0.0f));
            impl_->mixer->AddSource(created.get());
            it = impl_->peers.emplace(peer, std::move(created)).first;
            MELLO_LOG_INFO("playout", "peer '%s': NetEQ created (ssrc %08x, streams=%zu)",
                           peer.c_str(), rtp.ssrc, impl_->peers.size());
        }
        PeerStream& s = *it->second;
        if (!s.has_last_timestamp || static_cast<int32_t>(rtp.timestamp - s.last_timestamp) > 0) {
            s.last_timestamp = rtp.timestamp;
        }
        s.has_last_timestamp = true;
        stream = it->second;
    }

    webrtc::RTPHeader header;
    header.payloadType = rtp.payload_type;
    header.sequenceNumber = rtp.sequence;
    header.timestamp = rtp.timestamp;
    header.ssrc = rtp.ssrc;
    const int rc = stream->neteq().InsertPacket(
        header, rtc::ArrayView<const uint8_t>(payload, payload_size),
        webrtc::Timestamp::Millis(receive_time_ms));
    c.packets_inserted.fetch_add(1, std::memory_order_relaxed);
    if (rc != webrtc::NetEq::kOK) {
        const uint64_t n = c.insert_errors.fetch_add(1, std::memory_order_relaxed) + 1;
        if (n <= 5 || (n % 500) == 0) {
            MELLO_LOG_WARN("playout", "peer '%s': InsertPacket failed (#%llu, seq=%u ts=%u size=%zu)",
                           peer.c_str(), static_cast<unsigned long long>(n), rtp.sequence,
                           rtp.timestamp, payload_size);
        }
        return false;
    }
    return true;
}

bool VoicePlayout::insert_rtp(const std::string& peer, const uint8_t* packet, size_t size,
                              int64_t receive_time_ms) {
    RtpInfo rtp;
    size_t offset = 0, payload_size = 0;
    if (!parse_rtp(packet, size, &rtp, &offset, &payload_size)) {
        impl_->counters.packets_rejected.fetch_add(1, std::memory_order_relaxed);
        return false;
    }
    return insert(peer, rtp, packet + offset, payload_size, receive_time_ms);
}

void VoicePlayout::remove_peer(const std::string& peer) {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    auto it = impl_->peers.find(peer);
    if (it == impl_->peers.end()) return;
    impl_->retire_locked(it->second);
    impl_->peers.erase(it);
    MELLO_LOG_INFO("playout", "peer '%s': NetEQ destroyed (streams=%zu)", peer.c_str(),
                   impl_->peers.size());
}

size_t VoicePlayout::clear() {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    const size_t n = impl_->peers.size();
    for (auto& [id, stream] : impl_->peers) impl_->retire_locked(stream);
    impl_->peers.clear();
    return n;
}

void VoicePlayout::set_peer_volume(const std::string& peer, float gain) {
    if (!std::isfinite(gain)) return;
    gain = std::clamp(gain, 0.0f, kMaxPeerVolume);
    std::lock_guard<std::mutex> lock(impl_->mutex);
    impl_->volumes[peer] = gain;
    auto it = impl_->peers.find(peer);
    if (it != impl_->peers.end()) it->second->volume.store(gain, std::memory_order_relaxed);
}

float VoicePlayout::peer_volume(const std::string& peer) const {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    return impl_->setting_locked(impl_->volumes, peer, 1.0f);
}

void VoicePlayout::set_peer_pan(const std::string& peer, float pan) {
    if (!std::isfinite(pan)) return;
    pan = std::clamp(pan, -1.0f, 1.0f);
    std::lock_guard<std::mutex> lock(impl_->mutex);
    impl_->pans[peer] = pan;
    auto it = impl_->peers.find(peer);
    if (it != impl_->peers.end()) it->second->pan.store(pan, std::memory_order_relaxed);
}

float VoicePlayout::peer_pan(const std::string& peer) const {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    return impl_->setting_locked(impl_->pans, peer, 0.0f);
}

void VoicePlayout::set_output_gain(float gain) {
    if (!std::isfinite(gain)) return;
    impl_->counters.output_gain.store(std::clamp(gain, 0.0f, kMaxPeerVolume),
                                      std::memory_order_relaxed);
}

int VoicePlayout::mix(int16_t* out) {
    auto& c = impl_->counters;
    c.contributing = 0;
    webrtc::AudioFrame& mixed = impl_->mixed;
    impl_->mixer->Mix(kChannels, &mixed);
    const size_t total = static_cast<size_t>(kFrameSamples) * kChannels;
    if (c.contributing == 0 || mixed.muted() ||
        mixed.samples_per_channel_ != static_cast<size_t>(kFrameSamples) ||
        mixed.num_channels_ != static_cast<size_t>(kChannels) ||
        mixed.sample_rate_hz_ != kSampleRateHz) {
        std::fill(out, out + total, int16_t{0});
        return 0;
    }
    std::copy(mixed.data(), mixed.data() + total, out);
    return c.contributing;
}

PlayoutStats VoicePlayout::stats() const {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    PlayoutStats s = impl_->retired;
    float target = 0.0f, filtered = 0.0f, buffer = 0.0f, hold = 0.0f;
    for (auto& [id, stream] : impl_->peers) {
        webrtc::NetEq& neteq = stream->neteq();
        add_lifetime(s, neteq.GetLifetimeStatistics(), neteq.GetOperationsAndState());
        s.talk_spurts += stream->spurts->talk_spurts.load(std::memory_order_relaxed);
        hold += static_cast<float>(stream->spurts->hold_ms.load(std::memory_order_relaxed));
        target += static_cast<float>(neteq.TargetDelayMs());
        filtered += static_cast<float>(neteq.FilteredCurrentDelayMs());
        buffer += static_cast<float>(neteq.CurrentNetworkStatistics().current_buffer_size_ms);
    }
    s.peers = static_cast<int>(impl_->peers.size());
    if (s.peers > 0) {
        s.target_delay_ms = target / static_cast<float>(s.peers);
        s.filtered_delay_ms = filtered / static_cast<float>(s.peers);
        s.buffer_ms = buffer / static_cast<float>(s.peers);
        s.spurt_hold_ms = hold / static_cast<float>(s.peers);
    }
    const auto& c = impl_->counters;
    s.packets_inserted = c.packets_inserted.load(std::memory_order_relaxed);
    s.insert_errors = c.insert_errors.load(std::memory_order_relaxed);
    s.get_audio_errors = c.get_audio_errors.load(std::memory_order_relaxed);
    s.packets_rejected = c.packets_rejected.load(std::memory_order_relaxed);
    s.stream_resets = c.stream_resets.load(std::memory_order_relaxed);
    s.late_frames = c.late_frames.load(std::memory_order_relaxed);
    return s;
}

uint64_t VoicePlayout::late_frames() const {
    return impl_->counters.late_frames.load(std::memory_order_relaxed);
}

int VoicePlayout::peer_count() const {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    return static_cast<int>(impl_->peers.size());
}

} // namespace mello::audio
