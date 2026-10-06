// Stage 3 spike: WebRTC NetEQ (M131) and AudioMixerImpl as the voice playout
// engine. This suite drives the vendored NetEQ with real Opus packets from the
// project's OpusEnc (20 ms, FEC on, DTX on: the settings of opus_codec.cpp),
// real RTP sequence numbers and timestamps, a SimulatedClock and GetAudio every
// 10 ms. Nothing here touches AudioPipeline.
//
// Each scenario prints its numbers with the prefix "[neteq-spike]" and fails
// when NetEQ misbehaves. Run one scenario alone with
//   mello_neteq_spike_tests --gtest_filter=NetEqSpike.<Name>
//
// Thread model: single thread. The simulation calls InsertPacket and GetAudio
// from the test thread in simulated-time order.

#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <limits>
#include <memory>
#include <new>
#include <random>
#include <string>
#include <vector>

#include "api/array_view.h"
#include "api/audio/audio_frame.h"
#include "api/audio/audio_mixer.h"
#include "api/audio_codecs/audio_decoder_factory_template.h"
#include "api/audio_codecs/audio_format.h"
#include "api/audio_codecs/opus/audio_decoder_opus.h"
#include "api/environment/environment.h"
#include "api/environment/environment_factory.h"
#include "api/field_trials_view.h"
#include "api/neteq/default_neteq_factory.h"
#include "api/neteq/neteq.h"
#include "api/rtp_headers.h"
#include "api/units/timestamp.h"
#include "modules/audio_mixer/audio_mixer_impl.h"
#include "system_wrappers/include/clock.h"

#include "audio/opus_codec.hpp"

#ifdef _WIN32
#include <windows.h>
#include <psapi.h>
#endif

// ---------------------------------------------------------------------------
// Heap counter. This executable replaces the global operator new and delete
// so the RAM measurement can count the C++ heap that one NetEQ instance holds.
// It counts live bytes only. malloc (the Opus decoder state, AlignedMalloc)
// is not counted here; the process private-bytes delta covers it.
// ---------------------------------------------------------------------------
namespace {
std::atomic<int64_t> g_live_heap_bytes{0};
constexpr size_t kHeapHeader = 16;  // keeps the user block 16-byte aligned
}  // namespace

void* operator new(size_t size) {
    void* raw = std::malloc(size + kHeapHeader);
    if (!raw) throw std::bad_alloc();
    *static_cast<size_t*>(raw) = size;
    g_live_heap_bytes.fetch_add(static_cast<int64_t>(size), std::memory_order_relaxed);
    return static_cast<char*>(raw) + kHeapHeader;
}

void operator delete(void* p) noexcept {
    if (!p) return;
    void* raw = static_cast<char*>(p) - kHeapHeader;
    g_live_heap_bytes.fetch_sub(static_cast<int64_t>(*static_cast<size_t*>(raw)),
                                std::memory_order_relaxed);
    std::free(raw);
}

void operator delete(void* p, size_t) noexcept { operator delete(p); }

namespace {

using webrtc::AudioFrame;
using webrtc::NetEq;

constexpr int kSampleRateHz = 48000;
constexpr int kFrameMs = 20;
constexpr int kSamplesPerFrame = kSampleRateHz * kFrameMs / 1000;  // 960
constexpr int kOpusPayloadType = 111;
constexpr int kCnPayloadType = 13;
constexpr int64_t kTickUs = 10'000;  // GetAudio period

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

std::string fixture_path(const char* name) {
    return std::string(MELLO_VOICE_FIXTURE_DIR) + "/" + name;
}

// Reads a 16-bit PCM mono WAV. Returns an empty vector on a format it does
// not handle.
std::vector<int16_t> load_wav_mono16(const std::string& path) {
    std::ifstream f(path, std::ios::binary);
    if (!f) return {};
    std::vector<char> bytes((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
    if (bytes.size() < 12 || std::memcmp(bytes.data(), "RIFF", 4) != 0 ||
        std::memcmp(bytes.data() + 8, "WAVE", 4) != 0) {
        return {};
    }
    auto u16 = [&](size_t o) {
        return static_cast<uint16_t>(static_cast<uint8_t>(bytes[o]) |
                                     (static_cast<uint8_t>(bytes[o + 1]) << 8));
    };
    auto u32 = [&](size_t o) {
        return static_cast<uint32_t>(u16(o)) | (static_cast<uint32_t>(u16(o + 2)) << 16);
    };
    size_t pos = 12;
    int channels = 0, bits = 0;
    uint32_t rate = 0;
    while (pos + 8 <= bytes.size()) {
        const uint32_t len = u32(pos + 4);
        const char* id = bytes.data() + pos;
        if (std::memcmp(id, "fmt ", 4) == 0) {
            channels = u16(pos + 10);
            rate = u32(pos + 12);
            bits = u16(pos + 22);
        } else if (std::memcmp(id, "data", 4) == 0) {
            if (channels != 1 || bits != 16 || rate != kSampleRateHz) return {};
            const size_t n = std::min<size_t>(len, bytes.size() - pos - 8) / 2;
            std::vector<int16_t> out(n);
            std::memcpy(out.data(), bytes.data() + pos + 8, n * 2);
            return out;
        }
        pos += 8 + len + (len & 1);
    }
    return {};
}

// Scales the clip so its peak sample sits at full scale.
std::vector<int16_t> normalize_to_full_scale(std::vector<int16_t> pcm) {
    int peak = 1;
    for (int16_t s : pcm) peak = std::max(peak, std::abs(static_cast<int>(s)));
    const double gain = 32767.0 / peak;
    for (int16_t& s : pcm) {
        s = static_cast<int16_t>(std::clamp(std::lround(s * gain), -32768L, 32767L));
    }
    return pcm;
}

// One pass of the clip through the project's encoder. Payloads only; the
// scenario assigns sequence numbers and timestamps. DTX frames (1 or 2 bytes)
// stay in, because AudioPipeline sends every frame that encodes to > 0 bytes.
std::vector<std::vector<uint8_t>> encode_clip(const std::vector<int16_t>& pcm) {
    mello::audio::OpusEnc enc;
    EXPECT_TRUE(enc.initialize());  // 48 kHz mono, 64 kbit/s, VOIP: FEC, DTX, 5 % loss hint
    std::vector<std::vector<uint8_t>> out;
    uint8_t buf[mello::audio::MAX_PACKET_SIZE];
    for (size_t off = 0; off + kSamplesPerFrame <= pcm.size(); off += kSamplesPerFrame) {
        const int n = enc.encode(pcm.data() + off, kSamplesPerFrame, buf, sizeof(buf));
        EXPECT_GE(n, 0);
        if (n > 0) out.emplace_back(buf, buf + n);
    }
    return out;
}

const std::vector<std::vector<uint8_t>>& speech_packets() {
    static const auto packets = [] {
        auto pcm = load_wav_mono16(fixture_path("m1_davison.wav"));
        return encode_clip(pcm);
    }();
    return packets;
}

// ---------------------------------------------------------------------------
// Network and sender model
// ---------------------------------------------------------------------------

struct SimPacket {
    uint16_t seq = 0;
    uint32_t ts = 0;
    const std::vector<uint8_t>* payload = nullptr;
    int64_t send_us = 0;    // receiver clock
    int64_t arrive_us = 0;  // receiver clock
};

struct StreamSpec {
    int64_t duration_ms = 10'000;
    double drift_ppm = 0.0;  // > 0: the sender clock runs fast
    int64_t gap_start_ms = -1;  // sender VAD gate: no packets in the gap
    int64_t gap_len_ms = 0;
    double loss = 0.0;  // independent random loss
    // Network delay: base + uniform(0, jitter) ms. Inside the burst window the
    // jitter is burst_jitter_ms.
    double base_delay_ms = 20.0;
    double jitter_ms = 2.0;
    int64_t burst_start_ms = -1;
    int64_t burst_len_ms = 0;
    double burst_jitter_ms = 0.0;
    // true: one network path, packets arrive in send order (a queue delays
    // a packet, it does not pass it). false: each packet takes its own delay,
    // so jitter above 20 ms reorders packets.
    bool fifo = true;
    uint32_t seed = 1;
};

// Builds the arrival schedule of one sender. RTP timestamps advance 960 per
// 20 ms of sender time, also through the gap. Sequence numbers advance only
// for frames that are sent, like AudioPipeline (sequence_++ after encode).
std::vector<SimPacket> make_stream(const StreamSpec& s,
                                   const std::vector<std::vector<uint8_t>>& corpus,
                                   int* lost_out = nullptr) {
    std::mt19937 rng(s.seed);
    std::uniform_real_distribution<double> uni(0.0, 1.0);
    std::vector<SimPacket> out;
    const int frames = static_cast<int>(s.duration_ms / kFrameMs);
    uint16_t seq = 1000;
    const uint32_t ts0 = 123456;
    int lost = 0;
    int64_t last_arrive_us = 0;
    for (int i = 0; i < frames; ++i) {
        const int64_t sender_ms = static_cast<int64_t>(i) * kFrameMs;
        if (s.gap_start_ms >= 0 && sender_ms >= s.gap_start_ms &&
            sender_ms < s.gap_start_ms + s.gap_len_ms) {
            continue;  // gated: nothing encoded, nothing sent
        }
        SimPacket p;
        p.seq = seq++;
        p.ts = ts0 + static_cast<uint32_t>(i) * kSamplesPerFrame;
        p.payload = &corpus[static_cast<size_t>(i) % corpus.size()];
        // Sender clock fast by drift_ppm: frame i leaves at i*20 ms of sender
        // time, which is earlier on the receiver clock.
        p.send_us = static_cast<int64_t>(std::llround(sender_ms * 1000.0 / (1.0 + s.drift_ppm * 1e-6)));
        const bool in_burst = s.burst_start_ms >= 0 && sender_ms >= s.burst_start_ms &&
                              sender_ms < s.burst_start_ms + s.burst_len_ms;
        const double jitter = in_burst ? s.burst_jitter_ms : s.jitter_ms;
        const double delay_ms = s.base_delay_ms + uni(rng) * jitter;
        p.arrive_us = p.send_us + static_cast<int64_t>(delay_ms * 1000.0);
        if (s.fifo) p.arrive_us = std::max(p.arrive_us, last_arrive_us);
        const bool drop = uni(rng) < s.loss;
        if (drop) {
            ++lost;
            continue;
        }
        last_arrive_us = p.arrive_us;
        out.push_back(p);
    }
    std::stable_sort(out.begin(), out.end(),
                     [](const SimPacket& a, const SimPacket& b) { return a.arrive_us < b.arrive_us; });
    if (lost_out) *lost_out = lost;
    return out;
}

// ---------------------------------------------------------------------------
// Receiver: NetEQ instances on one SimulatedClock
// ---------------------------------------------------------------------------

// The decoder factory offers Opus only. Comfort noise (payload "CN") has no
// AudioDecoder: NetEQ's DecoderDatabase handles it internally with the
// vendored webrtc_cng. This avoids CreateBuiltinAudioDecoderFactory, which
// links every codec.
rtc::scoped_refptr<webrtc::AudioDecoderFactory> make_decoder_factory() {
    return webrtc::CreateAudioDecoderFactory<webrtc::AudioDecoderOpus>();
}

// Field trials for one Sim. NetEQ reads its delay manager tuning from the
// key "WebRTC-Audio-NetEqDelayManagerConfig".
class SpikeFieldTrials : public webrtc::FieldTrialsView {
public:
    explicit SpikeFieldTrials(std::string delay_manager_config)
        : delay_manager_config_(std::move(delay_manager_config)) {}
    std::string Lookup(absl::string_view key) const override {
        if (key == "WebRTC-Audio-NetEqDelayManagerConfig") return delay_manager_config_;
        return "";
    }

private:
    std::string delay_manager_config_;
};

std::unique_ptr<NetEq> make_neteq(const webrtc::Environment& env) {
    NetEq::Config config;
    config.sample_rate_hz = kSampleRateHz;
    auto neteq = webrtc::DefaultNetEqFactory().Create(env, config, make_decoder_factory());
    EXPECT_TRUE(neteq->RegisterPayloadType(kOpusPayloadType,
                                           webrtc::SdpAudioFormat("opus", 48000, 2, {{"stereo", "0"}})));
    EXPECT_TRUE(neteq->RegisterPayloadType(kCnPayloadType, webrtc::SdpAudioFormat("CN", 48000, 1)));
    return neteq;
}

struct Lane {
    std::unique_ptr<NetEq> neteq;
    std::vector<SimPacket> packets;
    size_t next = 0;
    uint32_t ssrc = 0;
    double drift_ppm = 0.0;
    uint32_t ts0 = 123456;
    int insert_errors = 0;
};

struct Timing {
    int64_t insert_ns = 0;
    int64_t insert_calls = 0;
    int64_t get_ns = 0;
    int64_t get_calls = 0;
};

class Sim {
public:
    // delay_manager_config: the value of the NetEQ delay manager field trial,
    // for example "use_reorder_optimizer:false". Empty: upstream defaults.
    explicit Sim(std::string delay_manager_config = "")
        : clock_(webrtc::Timestamp::Seconds(1000)),
          trials_(std::make_unique<SpikeFieldTrials>(std::move(delay_manager_config))),
          env_(webrtc::CreateEnvironment(static_cast<webrtc::Clock*>(&clock_),
                                         static_cast<const webrtc::FieldTrialsView*>(trials_.get()))) {}

    Lane& add_lane(std::vector<SimPacket> packets, uint32_t ssrc, double drift_ppm = 0.0) {
        auto lane = std::make_unique<Lane>();
        lane->neteq = make_neteq(env_);
        lane->packets = std::move(packets);
        lane->ssrc = ssrc;
        lane->drift_ppm = drift_ppm;
        lanes_.push_back(std::move(lane));
        return *lanes_.back();
    }

    // Inserts every packet that arrives at or before t_us, in arrival order
    // across lanes, with the clock at each arrival time. Then moves the clock
    // to t_us.
    void deliver_until(int64_t t_us) {
        for (;;) {
            Lane* best = nullptr;
            for (auto& l : lanes_) {
                if (l->next < l->packets.size() && l->packets[l->next].arrive_us <= t_us &&
                    (!best || l->packets[l->next].arrive_us < best->packets[best->next].arrive_us)) {
                    best = l.get();
                }
            }
            if (!best) break;
            const SimPacket& p = best->packets[best->next++];
            advance_to(p.arrive_us);
            webrtc::RTPHeader h;
            h.payloadType = kOpusPayloadType;
            h.sequenceNumber = p.seq;
            h.timestamp = p.ts;
            h.ssrc = best->ssrc;
            const auto t0 = std::chrono::steady_clock::now();
            const int rc = best->neteq->InsertPacket(
                h, rtc::ArrayView<const uint8_t>(p.payload->data(), p.payload->size()),
                clock_.CurrentTime());
            timing_.insert_ns += std::chrono::duration_cast<std::chrono::nanoseconds>(
                                     std::chrono::steady_clock::now() - t0).count();
            timing_.insert_calls++;
            if (rc != NetEq::kOK) best->insert_errors++;
        }
        advance_to(t_us);
    }

    // One 10 ms GetAudio on a lane. Returns the NetEQ return code.
    int get_audio(Lane& lane, AudioFrame* frame) {
        bool muted = false;
        const auto t0 = std::chrono::steady_clock::now();
        const int rc = lane.neteq->GetAudio(frame, &muted);
        timing_.get_ns += std::chrono::duration_cast<std::chrono::nanoseconds>(
                              std::chrono::steady_clock::now() - t0).count();
        timing_.get_calls++;
        return rc;
    }

    // Mouth-to-ear latency of the sample NetEQ played last: receiver time now
    // minus the receiver time the sender captured it. Includes the network
    // delay; excludes device buffers.
    double playout_latency_ms(const Lane& lane) {
        auto ts = lane.neteq->GetPlayoutTimestamp();
        if (!ts) return -1.0;
        const int32_t rel = static_cast<int32_t>(*ts - lane.ts0);
        const double sender_ms = rel / 48.0;
        const double recv_ms = sender_ms / (1.0 + lane.drift_ppm * 1e-6);
        return now_us() / 1000.0 - recv_ms;
    }

    int64_t now_us() { return clock_.TimeInMicroseconds() - start_us_; }
    const Timing& timing() const { return timing_; }
    std::vector<std::unique_ptr<Lane>>& lanes() { return lanes_; }

private:
    void advance_to(int64_t t_us) {
        const int64_t target = start_us_ + t_us;
        const int64_t cur = clock_.TimeInMicroseconds();
        if (target > cur) clock_.AdvanceTimeMicroseconds(target - cur);
    }

    webrtc::SimulatedClock clock_;
    const int64_t start_us_ = 1000LL * 1000 * 1000;
    std::unique_ptr<SpikeFieldTrials> trials_;
    webrtc::Environment env_;
    std::vector<std::unique_ptr<Lane>> lanes_;
    Timing timing_;
};

struct SpeechTypeCounts {
    int normal = 0, plc = 0, cng = 0, plccng = 0, codec_plc = 0, undefined = 0;
    void add(AudioFrame::SpeechType t) {
        switch (t) {
            case AudioFrame::kNormalSpeech: ++normal; break;
            case AudioFrame::kPLC: ++plc; break;
            case AudioFrame::kCNG: ++cng; break;
            case AudioFrame::kPLCCNG: ++plccng; break;
            case AudioFrame::kCodecPLC: ++codec_plc; break;
            default: ++undefined; break;
        }
    }
    int total() const { return normal + plc + cng + plccng + codec_plc + undefined; }
};

struct DelaySample {
    double t_ms;
    int target_ms;
    int filtered_ms;
    int buffer_ms;
    double latency_ms;
};

DelaySample sample_delay(Sim& sim, Lane& lane) {
    const auto ns = lane.neteq->CurrentNetworkStatistics();
    return {sim.now_us() / 1000.0, lane.neteq->TargetDelayMs(), lane.neteq->FilteredCurrentDelayMs(),
            ns.current_buffer_size_ms, sim.playout_latency_ms(lane)};
}

void print_delay(const char* scenario, const char* label, const DelaySample& d) {
    std::printf("[neteq-spike] %s %-22s t=%7.0f ms target=%3d filtered_current=%3d buffer=%3d latency=%6.1f ms\n",
                scenario, label, d.t_ms, d.target_ms, d.filtered_ms, d.buffer_ms, d.latency_ms);
}

double q14(uint16_t v) { return v / 16384.0; }

// Runs one lane for duration_ms of 10 ms ticks. on_tick runs after each
// GetAudio. Fails the test on a GetAudio error or a frame that is not
// 10 ms at 48 kHz mono.
template <typename F>
void run_lane(Sim& sim, Lane& lane, int64_t duration_ms, SpeechTypeCounts* counts, F&& on_tick) {
    AudioFrame frame;
    const int64_t ticks = duration_ms * 1000 / kTickUs;
    int errors = 0;
    for (int64_t i = 1; i <= ticks; ++i) {
        sim.deliver_until(i * kTickUs);
        if (sim.get_audio(lane, &frame) != NetEq::kOK) ++errors;
        ASSERT_EQ(frame.samples_per_channel_, 480u);
        ASSERT_EQ(frame.sample_rate_hz_, kSampleRateHz);
        ASSERT_EQ(frame.num_channels_, 1u);
        if (counts) counts->add(frame.speech_type_);
        on_tick(i * kTickUs / 1000, frame);
    }
    EXPECT_EQ(errors, 0) << "GetAudio returned an error";
    EXPECT_EQ(lane.insert_errors, 0) << "InsertPacket returned an error";
}

void print_counts(const char* scenario, const SpeechTypeCounts& c) {
    std::printf("[neteq-spike] %s speech types (10 ms frames): normal=%d plc=%d cng=%d plccng=%d codec_plc=%d undefined=%d total=%d\n",
                scenario, c.normal, c.plc, c.cng, c.plccng, c.codec_plc, c.undefined, c.total());
}

// Captures the delay at the first 10 ms tick at or after at_ms that has a
// playout latency. NetEQ reports no playout timestamp while it plays comfort
// noise for an Opus DTX period, so a fixed tick can miss.
struct Probe {
    explicit Probe(int64_t at) : at_ms(at) {}
    int64_t at_ms;
    bool done = false;
    DelaySample d{};
    void offer(int64_t t_ms, Sim& sim, Lane& lane) {
        if (done || t_ms < at_ms) return;
        const DelaySample s = sample_delay(sim, lane);
        if (s.latency_ms < 0) return;
        d = s;
        done = true;
    }
};

struct BurstResult {
    DelaySample before{}, peak{}, after5{}, after10{}, after15{};
    bool complete = false;
    std::string trace;
    uint64_t accelerated_samples = 0;
    // Time from the burst end until the target and the buffer are both back
    // within 20 ms of their values before the burst. -1: not back by the end.
    double return_s = -1.0;
};

constexpr int64_t kBurstStartMs = 10'000;
constexpr int64_t kBurstEndMs = 13'000;

// 10 s calm, a 3 s jitter burst of 0..120 ms, 30 s calm.
BurstResult run_burst(bool fifo, const std::string& delay_manager_config) {
    StreamSpec spec;
    spec.duration_ms = 43'000;
    spec.burst_start_ms = 10'000;
    spec.burst_len_ms = 3'000;
    spec.burst_jitter_ms = 120.0;
    spec.fifo = fifo;
    spec.seed = 3;
    Sim sim(delay_manager_config);
    Lane& lane = sim.add_lane(make_stream(spec, speech_packets()), 0x3333);
    BurstResult r;
    Probe before(10'000), after5(18'000), after10(23'000), after15(28'000);
    char buf[64];
    int64_t back_since_ms = -1;
    run_lane(sim, lane, spec.duration_ms, nullptr, [&](int64_t t_ms, const AudioFrame&) {
        before.offer(t_ms, sim, lane);
        after5.offer(t_ms, sim, lane);
        after10.offer(t_ms, sim, lane);
        after15.offer(t_ms, sim, lane);
        if (t_ms % 100 == 0 && t_ms > kBurstEndMs && before.done) {
            const auto ns = lane.neteq->CurrentNetworkStatistics();
            const bool back = lane.neteq->TargetDelayMs() <= before.d.target_ms + 20 &&
                              ns.current_buffer_size_ms <= before.d.buffer_ms + 20;
            if (!back) back_since_ms = -1;
            else if (back_since_ms < 0) back_since_ms = t_ms;
        }
        if (t_ms % 500 != 0) return;
        const DelaySample d = sample_delay(sim, lane);
        if (t_ms > kBurstStartMs && t_ms <= kBurstEndMs + 5'000 && d.target_ms > r.peak.target_ms) r.peak = d;
        if (t_ms >= 9'000 && t_ms % 1000 == 0) {
            std::snprintf(buf, sizeof(buf), " %.1f:%d/%d", t_ms / 1000.0, d.target_ms, d.buffer_ms);
            r.trace += buf;
        }
    });
    r.before = before.d;
    r.after5 = after5.d;
    r.after10 = after10.d;
    r.after15 = after15.d;
    r.complete = before.done && after5.done && after10.done && after15.done;
    r.accelerated_samples = lane.neteq->GetLifetimeStatistics().removed_samples_for_acceleration;
    if (back_since_ms >= 0) r.return_s = (back_since_ms - kBurstEndMs) / 1000.0;
    return r;
}

void print_burst(const char* scenario, const BurstResult& r) {
    print_delay(scenario, "before burst (10 s)", r.before);
    print_delay(scenario, "peak target", r.peak);
    print_delay(scenario, "burst end + 5 s", r.after5);
    print_delay(scenario, "burst end + 10 s", r.after10);
    print_delay(scenario, "burst end + 15 s", r.after15);
    std::printf("[neteq-spike] %s trace (s:target/buffer ms):%s\n", scenario, r.trace.c_str());
    std::printf("[neteq-spike] %s removed_samples_for_acceleration=%llu return_after_burst=%.1f s\n", scenario,
                (unsigned long long)r.accelerated_samples, r.return_s);
}

}  // namespace

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

TEST(NetEqSpike, CorpusEncodes) {
    const auto& packets = speech_packets();
    ASSERT_GT(packets.size(), 500u) << "m1_davison.wav did not load or encode";
    int dtx = 0;
    for (const auto& p : packets) dtx += p.size() <= 2 ? 1 : 0;
    std::printf("[neteq-spike] corpus m1_davison.wav: %zu packets, %d DTX packets (<= 2 bytes)\n",
                packets.size(), dtx);
}

// Clean network: 20 ms base delay, 0..2 ms jitter.
TEST(NetEqSpike, CleanSteadyDelay) {
    StreamSpec spec;
    spec.duration_ms = 10'000;
    Sim sim;
    Lane& lane = sim.add_lane(make_stream(spec, speech_packets()), 0x1111);
    SpeechTypeCounts counts;
    Probe at5(5'000);
    int min_buf = 1000, max_buf = 0;
    run_lane(sim, lane, spec.duration_ms, &counts, [&](int64_t t_ms, const AudioFrame&) {
        at5.offer(t_ms, sim, lane);
        if (t_ms > 5'000 && t_ms % 100 == 0) {
            const int b = lane.neteq->CurrentNetworkStatistics().current_buffer_size_ms;
            min_buf = std::min(min_buf, b);
            max_buf = std::max(max_buf, b);
        }
    });
    print_delay("clean", "after 5 s", at5.d);
    print_counts("clean", counts);
    const auto lt = lane.neteq->GetLifetimeStatistics();
    std::printf("[neteq-spike] clean concealed_samples=%llu concealment_events=%llu buffer 5..10 s: min=%d max=%d ms\n",
                (unsigned long long)lt.concealed_samples, (unsigned long long)lt.concealment_events,
                min_buf, max_buf);

    ASSERT_TRUE(at5.done);
    EXPECT_LE(at5.d.target_ms, 80);
    EXPECT_LE(at5.d.latency_ms, 80.0);
    EXPECT_LE(max_buf - min_buf, 40) << "buffer is not steady on a clean network";
    EXPECT_EQ(lt.concealment_events, 0u) << "concealment on a clean network";
}

// 5 % independent random loss.
TEST(NetEqSpike, RandomLossFivePercent) {
    StreamSpec spec;
    spec.duration_ms = 30'000;
    spec.loss = 0.05;
    spec.seed = 7;
    int lost = 0;
    Sim sim;
    Lane& lane = sim.add_lane(make_stream(spec, speech_packets(), &lost), 0x2222);
    SpeechTypeCounts counts;
    run_lane(sim, lane, spec.duration_ms, &counts, [](int64_t, const AudioFrame&) {});
    webrtc::NetEqNetworkStatistics ns{};
    lane.neteq->NetworkStatistics(&ns);
    const auto lt = lane.neteq->GetLifetimeStatistics();
    print_counts("loss5", counts);
    const double conceal_per_lost =
        lost > 0 ? static_cast<double>(lt.concealed_samples) / (lost * kSamplesPerFrame) : 0.0;
    std::printf("[neteq-spike] loss5 lost=%d of %d packets; concealed_samples=%llu (%.2f frames per lost packet) concealment_events=%llu\n",
                lost, static_cast<int>(spec.duration_ms / kFrameMs), (unsigned long long)lt.concealed_samples,
                conceal_per_lost, (unsigned long long)lt.concealment_events);
    std::printf("[neteq-spike] loss5 fec_packets_received=%llu fec_packets_discarded=%llu secondary_decoded_rate=%.4f secondary_discarded_rate=%.4f expand_rate=%.4f\n",
                (unsigned long long)lt.fec_packets_received, (unsigned long long)lt.fec_packets_discarded,
                q14(ns.secondary_decoded_rate), q14(ns.secondary_discarded_rate), q14(ns.expand_rate));
    print_delay("loss5", "end", sample_delay(sim, lane));

    EXPECT_GT(lost, 0);
    EXPECT_GT(counts.plc + counts.plccng + counts.codec_plc, 0) << "no concealment ran";
    EXPECT_GT(counts.normal, counts.total() * 9 / 10);
    EXPECT_GT(lt.fec_packets_received, 0u) << "Opus FEC was not parsed";
    EXPECT_GT(ns.secondary_decoded_rate, 0) << "no lost frame was recovered from Opus FEC";
    // One concealment per lost frame at most; FEC repairs most of them.
    EXPECT_LE(conceal_per_lost, 1.0);
}

// Jitter burst on one network path (packets queue, they do not pass each
// other): delay rises under the burst and accelerate brings it back down.
//
// The return time is set by the delay manager's memory, not by accelerate:
// once the target drops, the buffer follows within one second. Upstream
// defaults (forget_factor 0.983 per 500 ms) hold a 3 s burst for about 26 s.
// The tuning is a field trial string, not a code change.
TEST(NetEqSpike, JitterBurstThenCalm) {
    struct Run {
        const char* config;
        BurstResult r;
    };
    std::vector<Run> runs = {{"", {}}, {"forget_factor:0.8", {}}, {"forget_factor:0.7", {}},
                             {"forget_factor:0.6", {}}};
    for (auto& run : runs) {
        run.r = run_burst(/*fifo=*/true, run.config);
        print_burst((std::string("burst ") + (*run.config ? run.config : "default")).c_str(), run.r);
    }
    for (const auto& run : runs) {
        ASSERT_TRUE(run.r.complete) << run.config;
        EXPECT_GE(run.r.peak.target_ms, run.r.before.target_ms + 40)
            << run.config << ": target delay did not rise under the burst";
        EXPECT_GT(run.r.accelerated_samples, 0u) << run.config << ": no accelerate";
        EXPECT_GE(run.r.return_s, 0.0) << run.config << ": delay did not return within 30 s";
    }
    EXPECT_LE(runs[0].r.return_s, 30.0) << "upstream defaults";
    EXPECT_LE(runs[2].r.return_s, 6.0) << "forget_factor:0.7 did not return within 6 s";
}

// The same burst with independent per-packet delay, so packets reorder and
// the reorder optimizer also votes for a high target. Its upstream memory
// (reorder_forget_factor 0.9993 per packet) is longer still, so the tuning
// shortens both.
TEST(NetEqSpike, JitterBurstWithReorder) {
    const BurstResult def = run_burst(/*fifo=*/false, "");
    print_burst("burst-reorder default", def);
    const std::string tuned_cfg = "forget_factor:0.7,reorder_forget_factor:0.98";
    const BurstResult tuned = run_burst(/*fifo=*/false, tuned_cfg);
    print_burst(("burst-reorder " + tuned_cfg).c_str(), tuned);

    ASSERT_TRUE(def.complete && tuned.complete);
    EXPECT_GE(def.peak.target_ms, def.before.target_ms + 40);
    EXPECT_GE(tuned.return_s, 0.0) << "tuned delay did not return within 30 s";
    EXPECT_LE(tuned.return_s, 6.0) << "tuned delay did not return within 6 s";
}

// Sender clock +100 ppm fast. 60 s, then on to 600 s.
TEST(NetEqSpike, ClockDriftPlus100ppm) {
    StreamSpec spec;
    spec.duration_ms = 600'000;
    spec.drift_ppm = 100.0;
    spec.seed = 5;
    Sim sim;
    Lane& lane = sim.add_lane(make_stream(spec, speech_packets()), 0x4444, spec.drift_ppm);
    Probe start(5'000), at60(60'000), end(599'000);
    double max_latency = 0.0;
    run_lane(sim, lane, spec.duration_ms, nullptr, [&](int64_t t_ms, const AudioFrame&) {
        start.offer(t_ms, sim, lane);
        at60.offer(t_ms, sim, lane);
        end.offer(t_ms, sim, lane);
        if (t_ms >= 5'000 && t_ms % 100 == 0) max_latency = std::max(max_latency, sim.playout_latency_ms(lane));
    });
    print_delay("drift", "start (5 s)", start.d);
    print_delay("drift", "60 s", at60.d);
    print_delay("drift", "600 s", end.d);
    const auto lt = lane.neteq->GetLifetimeStatistics();
    std::printf("[neteq-spike] drift max latency 5..600 s=%.1f ms; removed_samples_for_acceleration=%llu (%.1f ms); sender excess over 600 s=%.1f ms\n",
                max_latency, (unsigned long long)lt.removed_samples_for_acceleration,
                lt.removed_samples_for_acceleration / 48.0, 600'000 * 100e-6);

    ASSERT_TRUE(start.done && at60.done && end.done);
    EXPECT_LE(std::abs(at60.d.latency_ms - start.d.latency_ms), 20.0);
    EXPECT_LE(std::abs(end.d.latency_ms - start.d.latency_ms), 20.0) << "delay drifts with the clock";
    EXPECT_LE(max_latency, start.d.latency_ms + 40.0);
}

// Sender VAD gate: 10 s of speech, a 2 s gap with no packets (timestamps
// advance, sequence numbers do not), then 10 s of speech.
TEST(NetEqSpike, TwoSecondGapNoBuildUp) {
    StreamSpec spec;
    spec.duration_ms = 22'000;
    spec.gap_start_ms = 10'000;
    spec.gap_len_ms = 2'000;
    spec.seed = 9;
    Sim sim;
    Lane& lane = sim.add_lane(make_stream(spec, speech_packets()), 0x5555);
    Probe before(9'800), after1(13'000), after5(17'000);
    webrtc::NetEqNetworkStatistics pre{}, gap{}, post{};
    SpeechTypeCounts gap_counts;
    run_lane(sim, lane, spec.duration_ms, nullptr, [&](int64_t t_ms, const AudioFrame& f) {
        before.offer(t_ms, sim, lane);
        after1.offer(t_ms, sim, lane);
        after5.offer(t_ms, sim, lane);
        if (t_ms > 10'000 && t_ms <= 12'000) gap_counts.add(f.speech_type_);
        if (t_ms == 10'000) lane.neteq->NetworkStatistics(&pre);   // rates over 0..10 s; resets
        if (t_ms == 12'000) lane.neteq->NetworkStatistics(&gap);   // rates over the gap
        if (t_ms == 14'000) lane.neteq->NetworkStatistics(&post);  // rates 12..14 s
    });
    print_delay("gap", "before gap", before.d);
    print_delay("gap", "gap end + 1 s", after1.d);
    print_delay("gap", "gap end + 5 s", after5.d);
    print_counts("gap(10..12 s)", gap_counts);
    auto rates = [](const char* label, const webrtc::NetEqNetworkStatistics& st) {
        std::printf("[neteq-spike] gap rates %-10s expand=%.4f speech_expand=%.4f preemptive=%.4f accelerate=%.4f\n",
                    label, q14(st.expand_rate), q14(st.speech_expand_rate), q14(st.preemptive_rate),
                    q14(st.accelerate_rate));
    };
    rates("0..10 s", pre);
    rates("10..12 s", gap);
    rates("12..14 s", post);

    ASSERT_TRUE(before.done && after5.done);
    EXPECT_LE(after5.d.latency_ms, before.d.latency_ms + 20.0) << "delay built up after the gap";
    EXPECT_LE(after5.d.target_ms, before.d.target_ms + 20);
}

// Two NetEQ instances with full-scale speech into AudioMixerImpl. The mixer's
// limiter (AGC2 Limiter in FrameCombiner) keeps the sum out of clipping.
namespace {
class NetEqSource : public webrtc::AudioMixer::Source {
public:
    NetEqSource(Sim& sim, Lane& lane) : sim_(sim), lane_(lane) {}
    AudioFrameInfo GetAudioFrameWithInfo(int sample_rate_hz, AudioFrame* frame) override {
        EXPECT_EQ(sample_rate_hz, kSampleRateHz);
        if (sim_.get_audio(lane_, frame) != NetEq::kOK) return AudioFrameInfo::kError;
        int peak = 0;
        const int16_t* d = frame->data();
        for (size_t i = 0; i < frame->samples_per_channel_; ++i) peak = std::max(peak, std::abs(int{d[i]}));
        source_peak = std::max(source_peak, peak);
        return AudioFrameInfo::kNormal;
    }
    int Ssrc() const override { return static_cast<int>(lane_.ssrc); }
    int PreferredSampleRate() const override { return kSampleRateHz; }
    int source_peak = 0;

private:
    Sim& sim_;
    Lane& lane_;
};
}  // namespace

namespace {
struct MixResult {
    int peak_a = 0, peak_b = 0, peak = 0;
    int64_t clipped = 0, naive_clipped = 0, samples = 0;
    double mix_us = 0.0;
};

// Mixes two NetEQ lanes through AudioMixerImpl (limiter on) for 10 s, and the
// same two streams as a plain int16 sum, the way AudioPipeline::mix_output
// sums today. The plain sum runs on a second simulation so it sees the same
// decoded audio.
MixResult run_mix(const std::vector<std::vector<uint8_t>>& corpus_a,
                  const std::vector<std::vector<uint8_t>>& corpus_b) {
    MixResult m;
    StreamSpec spec;
    spec.duration_ms = 10'000;
    const int64_t ticks = spec.duration_ms * 1000 / kTickUs;
    {
        Sim sim;
        spec.seed = 11;
        Lane& a = sim.add_lane(make_stream(spec, corpus_a), 0xAAAA);
        spec.seed = 12;
        Lane& b = sim.add_lane(make_stream(spec, corpus_b), 0xBBBB);
        NetEqSource src_a(sim, a), src_b(sim, b);
        auto mixer = webrtc::AudioMixerImpl::Create();  // limiter on
        EXPECT_TRUE(mixer->AddSource(&src_a));
        EXPECT_TRUE(mixer->AddSource(&src_b));
        AudioFrame mixed;
        int64_t mix_ns = 0;
        for (int64_t i = 1; i <= ticks; ++i) {
            sim.deliver_until(i * kTickUs);
            const auto t0 = std::chrono::steady_clock::now();
            mixer->Mix(1, &mixed);
            mix_ns += std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now() - t0).count();
            EXPECT_EQ(mixed.samples_per_channel_, 480u);
            EXPECT_EQ(mixed.sample_rate_hz_, kSampleRateHz);
            const int16_t* d = mixed.data();
            for (size_t k = 0; k < mixed.samples_per_channel_; ++k) {
                const int v = std::abs(int{d[k]});
                m.peak = std::max(m.peak, v);
                if (v >= 32767) ++m.clipped;
                ++m.samples;
            }
        }
        m.peak_a = src_a.source_peak;
        m.peak_b = src_b.source_peak;
        m.mix_us = mix_ns / 1000.0 / ticks;
        mixer->RemoveSource(&src_a);
        mixer->RemoveSource(&src_b);
    }
    {
        Sim ref;
        spec.seed = 11;
        Lane& ra = ref.add_lane(make_stream(spec, corpus_a), 0xAAAA);
        spec.seed = 12;
        Lane& rb = ref.add_lane(make_stream(spec, corpus_b), 0xBBBB);
        AudioFrame fa, fb;
        for (int64_t i = 1; i <= ticks; ++i) {
            ref.deliver_until(i * kTickUs);
            ref.get_audio(ra, &fa);
            ref.get_audio(rb, &fb);
            for (size_t k = 0; k < fa.samples_per_channel_; ++k) {
                const int sum = int{fa.data()[k]} + int{fb.data()[k]};
                if (sum >= 32767 || sum <= -32768) ++m.naive_clipped;
            }
        }
    }
    return m;
}

void print_mix(const char* label, const MixResult& m) {
    std::printf("[neteq-spike] mixer %s: source peaks %d/%d; mixed peak=%d (%.2f dBFS); clipped samples at the int16 ceiling: limiter=%lld plain sum=%lld of %lld; Mix()=%.1f us per 10 ms (includes 2 GetAudio)\n",
                label, m.peak_a, m.peak_b, m.peak, 20.0 * std::log10(std::max(m.peak, 1) / 32768.0),
                (long long)m.clipped, (long long)m.naive_clipped, (long long)m.samples, m.mix_us);
}
}  // namespace

// Two NetEQ instances with full-scale speech into AudioMixerImpl. Two cases:
// two talkers (m1 + f1), and one talker on both lanes in phase, which doubles
// every peak.
TEST(NetEqSpike, MixerLimiterTwoFullScaleSources) {
    const auto pcm_a = normalize_to_full_scale(load_wav_mono16(fixture_path("m1_davison.wav")));
    const auto pcm_b = normalize_to_full_scale(load_wav_mono16(fixture_path("f1_farris.wav")));
    ASSERT_FALSE(pcm_a.empty());
    ASSERT_FALSE(pcm_b.empty());
    const auto corpus_a = encode_clip(pcm_a);
    const auto corpus_b = encode_clip(pcm_b);

    const MixResult two = run_mix(corpus_a, corpus_b);
    print_mix("two talkers", two);
    const MixResult same = run_mix(corpus_a, corpus_a);
    print_mix("same talker in phase", same);

    for (const MixResult* m : {&two, &same}) {
        EXPECT_GE(m->peak_a, 30000) << "source is not near full scale";
        EXPECT_GE(m->peak_b, 30000) << "source is not near full scale";
    }
    EXPECT_GT(two.naive_clipped, 0) << "the plain sum of two talkers does not clip";
    EXPECT_EQ(two.clipped, 0) << "the mixer output clips with two talkers";
    // In phase the sum reaches +6 dBFS. The AGC2 limiter maps input above
    // +1 dBFS to 0 dBFS and clamps there, so a few samples sit at the int16
    // ceiling by design. The plain sum clips with up to 6 dB of overshoot.
    EXPECT_GT(same.naive_clipped, 1000) << "the plain sum does not clip, so the test proves nothing";
    EXPECT_LT(same.clipped * 20, same.naive_clipped) << "the limiter does not hold the in-phase sum";
}

// ---------------------------------------------------------------------------
// Cost: CPU per call (single instance, clean 10 s) and RAM per instance
// (50 instances after 10 s clean).
// ---------------------------------------------------------------------------

TEST(NetEqSpike, CostCpuAndRam) {
    StreamSpec spec;
    spec.duration_ms = 10'000;
    const auto& corpus = speech_packets();

    {
        Sim sim;
        Lane& lane = sim.add_lane(make_stream(spec, corpus), 0x6666);
        run_lane(sim, lane, spec.duration_ms, nullptr, [](int64_t, const AudioFrame&) {});
        const auto& t = sim.timing();
        const double ins_us = t.insert_ns / 1000.0 / std::max<int64_t>(t.insert_calls, 1);
        const double get_us = t.get_ns / 1000.0 / std::max<int64_t>(t.get_calls, 1);
        std::printf("[neteq-spike] cpu InsertPacket=%.2f us/call (%lld calls) GetAudio=%.2f us/call (%lld calls)\n",
                    ins_us, (long long)t.insert_calls, get_us, (long long)t.get_calls);
#ifdef NDEBUG
        // Generous bounds: the budget is one 10 ms audio callback for all peers.
        EXPECT_LT(get_us, 500.0);
        EXPECT_LT(ins_us, 200.0);
#endif
    }

    constexpr int kInstances = 50;
    std::vector<std::vector<SimPacket>> schedules;
    schedules.reserve(kInstances);
    for (int i = 0; i < kInstances; ++i) {
        spec.seed = 100 + i;
        schedules.push_back(make_stream(spec, corpus));
    }
    auto private_bytes = []() -> int64_t {
#ifdef _WIN32
        PROCESS_MEMORY_COUNTERS_EX pmc{};
        if (GetProcessMemoryInfo(GetCurrentProcess(), reinterpret_cast<PROCESS_MEMORY_COUNTERS*>(&pmc),
                                 sizeof(pmc))) {
            return static_cast<int64_t>(pmc.PrivateUsage);
        }
#endif
        return -1;
    };
    // The Sim (clock, environment) is shared; create it before the baseline.
    auto sim = std::make_unique<Sim>();
    const int64_t heap0 = g_live_heap_bytes.load();
    const int64_t priv0 = private_bytes();
    for (int i = 0; i < kInstances; ++i) {
        sim->add_lane(std::move(schedules[i]), 0x7000 + i);
    }
    const int64_t heap_created = g_live_heap_bytes.load();
    AudioFrame frame;
    for (int64_t i = 1; i <= spec.duration_ms * 1000 / kTickUs; ++i) {
        sim->deliver_until(i * kTickUs);
        for (auto& l : sim->lanes()) sim->get_audio(*l, &frame);
    }
    const int64_t heap1 = g_live_heap_bytes.load();
    const int64_t priv1 = private_bytes();
    // The packet schedules were allocated before heap0 and move into the
    // lanes without a new allocation, so the delta holds NetEQ and the Lane.
    const int64_t sched_bytes = 0;
    const double heap_per = (heap1 - heap0 - sched_bytes) / static_cast<double>(kInstances);
    const double heap_create_per = (heap_created - heap0 - sched_bytes) / static_cast<double>(kInstances);
    const double priv_per = priv0 >= 0 ? (priv1 - priv0) / static_cast<double>(kInstances) : -1.0;
    std::printf("[neteq-spike] ram per instance: C++ heap after create=%.1f KiB, after 10 s=%.1f KiB; process private bytes delta=%.1f KiB (includes malloc: Opus decoder state)\n",
                heap_create_per / 1024.0, heap_per / 1024.0, priv_per / 1024.0);
    EXPECT_LT(heap_per, 1024.0 * 1024.0) << "more than 1 MiB of heap per NetEQ instance";
}
