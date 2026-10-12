// VoicePlayout: one NetEQ per peer and the mixer (spec 10 section 5), and
// the RTP parser and the sinc resampler of the receive and device edges.
//
// Device-free and deterministic: the test inserts real Opus packets with
// explicit receive times and calls mix() every 10 ms of simulated time, the
// way the audio device thread does. NetEQ times playout by its GetAudio
// calls, so no clock and no sleep is involved.

#include <gtest/gtest.h>

#include "audio/opus_codec.hpp"
#include "audio/stream_resampler.hpp"
#include "audio/voice_playout.hpp"

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <functional>
#include <set>
#include <string>
#include <vector>

namespace {

using namespace mello::audio;

constexpr double kPi = 3.14159265358979323846;
constexpr int kStereoSamples = VoicePlayout::kFrameSamples * VoicePlayout::kChannels;

// One 20 ms Opus frame of a continuous tone, as the sender produces it.
class ToneSender {
public:
    explicit ToneSender(double hz = 300.0, double amplitude = 8000.0) : hz_(hz), amp_(amplitude) {
        EXPECT_TRUE(enc_.initialize());
    }

    std::vector<uint8_t> frame(uint32_t index) {
        int16_t pcm[FRAME_SIZE];
        for (int i = 0; i < FRAME_SIZE; ++i) {
            const double t = (static_cast<double>(index) * FRAME_SIZE + i) / SAMPLE_RATE;
            pcm[i] = static_cast<int16_t>(amp_ * std::sin(2.0 * kPi * hz_ * t));
        }
        uint8_t out[MAX_PACKET_SIZE];
        const int n = enc_.encode(pcm, FRAME_SIZE, out, MAX_PACKET_SIZE);
        EXPECT_GT(n, 0);
        return std::vector<uint8_t>(out, out + std::max(n, 0));
    }

private:
    OpusEnc enc_;
    double hz_;
    double amp_;
};

std::vector<uint8_t> make_rtp(uint16_t seq, uint32_t ts, uint32_t ssrc,
                              const std::vector<uint8_t>& payload, uint8_t pt = 111) {
    std::vector<uint8_t> p(12);
    p[0] = 0x80;
    p[1] = pt;
    p[2] = static_cast<uint8_t>(seq >> 8);
    p[3] = static_cast<uint8_t>(seq);
    for (int i = 0; i < 4; ++i) {
        p[4 + i] = static_cast<uint8_t>(ts >> (24 - 8 * i));
        p[8 + i] = static_cast<uint8_t>(ssrc >> (24 - 8 * i));
    }
    p.insert(p.end(), payload.begin(), payload.end());
    return p;
}

double rms(const int16_t* x, size_t n, size_t stride = 1, size_t offset = 0) {
    double sum = 0.0;
    for (size_t i = 0; i < n; ++i) {
        const double v = x[i * stride + offset];
        sum += v * v;
    }
    return std::sqrt(sum / static_cast<double>(n));
}

// A packet that reaches the receiver at arrival_ms.
struct Arrival {
    int64_t arrival_ms;
    std::string peer;
    RtpInfo rtp;
    std::vector<uint8_t> payload;
};

// Runs simulated time in 1 ms steps: inserts the due packets, mixes 10 ms
// every 10 ms. on_mix sees each mixed frame.
void play(VoicePlayout& playout, std::vector<Arrival> arrivals, int64_t end_ms,
          const std::function<void(int64_t, const int16_t*, int)>& on_mix) {
    std::stable_sort(arrivals.begin(), arrivals.end(),
                     [](const Arrival& a, const Arrival& b) { return a.arrival_ms < b.arrival_ms; });
    size_t next = 0;
    std::vector<int16_t> out(kStereoSamples);
    for (int64_t now = 0; now <= end_ms; ++now) {
        while (next < arrivals.size() && arrivals[next].arrival_ms <= now) {
            const Arrival& a = arrivals[next++];
            playout.insert(a.peer, a.rtp, a.payload.data(), a.payload.size(), now);
        }
        if (now % 10 == 0) {
            const int sources = playout.mix(out.data());
            on_mix(now, out.data(), sources);
        }
    }
}

// `count` frames of a tone sent every 20 ms with a 20 ms one-way delay.
std::vector<Arrival> tone_stream(ToneSender& tx, const std::string& peer, uint32_t count,
                                 uint32_t ssrc = 0x1234, uint32_t ts0 = 5000,
                                 uint16_t seq0 = 100, const std::set<uint32_t>& lost = {},
                                 int64_t start_ms = 0) {
    std::vector<Arrival> out;
    for (uint32_t i = 0; i < count; ++i) {
        auto payload = tx.frame(i);
        if (lost.count(i)) continue;
        Arrival a;
        a.arrival_ms = start_ms + static_cast<int64_t>(i) * 20 + 20;
        a.peer = peer;
        a.rtp.payload_type = 111;
        a.rtp.sequence = static_cast<uint16_t>(seq0 + i);
        a.rtp.timestamp = ts0 + i * 960;
        a.rtp.ssrc = ssrc;
        a.payload = std::move(payload);
        out.push_back(std::move(a));
    }
    return out;
}

VoicePlayout::Config default_config() {
    VoicePlayout::Config c;
    c.delay_manager_config = VoicePlayout::default_delay_manager_config();
    return c;
}

}  // namespace

// ---------------------------------------------------------------------------
// RTP parser
// ---------------------------------------------------------------------------

TEST(ParseRtp, FixedHeader) {
    const std::vector<uint8_t> payload{1, 2, 3};
    const auto pkt = make_rtp(0xBEEF, 0x01020304, 0xA1B2C3D4, payload);
    RtpInfo info;
    size_t off = 0, len = 0;
    ASSERT_TRUE(parse_rtp(pkt.data(), pkt.size(), &info, &off, &len));
    EXPECT_EQ(info.payload_type, 111);
    EXPECT_EQ(info.sequence, 0xBEEF);
    EXPECT_EQ(info.timestamp, 0x01020304u);
    EXPECT_EQ(info.ssrc, 0xA1B2C3D4u);
    EXPECT_EQ(off, 12u);
    EXPECT_EQ(len, 3u);
}

TEST(ParseRtp, CsrcExtensionAndPadding) {
    // 2 CSRCs, a one-word header extension, 2 bytes of padding.
    std::vector<uint8_t> p{0x80 | 0x20 | 0x10 | 0x02, 111, 0, 7, 0, 0, 3, 0xC0, 0, 0, 0, 9};
    p.insert(p.end(), 8, 0xEE);                 // CSRC list
    p.insert(p.end(), {0xBE, 0xDE, 0x00, 0x01});  // extension header, 1 word
    p.insert(p.end(), 4, 0x11);                 // extension data
    p.insert(p.end(), {0xAA, 0xBB, 0xCC});      // payload
    p.insert(p.end(), {0x00, 0x02});            // padding, count 2
    RtpInfo info;
    size_t off = 0, len = 0;
    ASSERT_TRUE(parse_rtp(p.data(), p.size(), &info, &off, &len));
    EXPECT_EQ(off, 12u + 8u + 8u);
    EXPECT_EQ(len, 3u);
    EXPECT_EQ(p[off], 0xAA);
    EXPECT_EQ(info.sequence, 7);
    EXPECT_EQ(info.timestamp, 960u);
    EXPECT_EQ(info.ssrc, 9u);
}

TEST(ParseRtp, RejectsBadPackets) {
    const std::vector<uint8_t> payload{1};
    auto good = make_rtp(1, 2, 3, payload);
    EXPECT_FALSE(parse_rtp(good.data(), 11, nullptr, nullptr, nullptr)) << "too short";
    auto v1 = good;
    v1[0] = 0x40;
    EXPECT_FALSE(parse_rtp(v1.data(), v1.size(), nullptr, nullptr, nullptr)) << "version 1";
    auto empty = make_rtp(1, 2, 3, {});
    EXPECT_FALSE(parse_rtp(empty.data(), empty.size(), nullptr, nullptr, nullptr)) << "no payload";
    auto ext = good;
    ext[0] |= 0x10;  // extension bit with no room for it
    EXPECT_FALSE(parse_rtp(ext.data(), ext.size(), nullptr, nullptr, nullptr));
}

// ---------------------------------------------------------------------------
// NetEQ per peer
// ---------------------------------------------------------------------------

// A clean stream plays the tone with no concealment and a small delay.
TEST(VoicePlayout, CleanStreamPlaysWithoutConcealment) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    int audible = 0;
    play(playout, tone_stream(tx, "a", 250), 250 * 20 + 200,
         [&](int64_t t, const int16_t* out, int sources) {
             if (t > 500 && t < 5000) {
                 EXPECT_EQ(sources, 1) << "t=" << t;
                 if (rms(out, VoicePlayout::kFrameSamples, 2) > 1000.0) ++audible;
             }
         });
    const auto s = playout.stats();
    EXPECT_GT(audible, 400);
    EXPECT_EQ(s.peers, 1);
    EXPECT_EQ(s.packets_inserted, 250u);
    EXPECT_EQ(s.insert_errors, 0u);
    EXPECT_EQ(s.get_audio_errors, 0u);
    EXPECT_EQ(s.stream_resets, 0u);
    EXPECT_LE(s.target_delay_ms, 80.0f);
    EXPECT_EQ(s.late_frames, 0u);
}

// The SFU reassigns the SSRC of a peer slot, and the new stream starts at a
// random timestamp. Here it is 1 s behind the old one: without a reset NetEQ
// would discard the new packets as old for about a second.
TEST(VoicePlayout, NewSsrcStartsANewStream) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    auto arrivals = tone_stream(tx, "a", 100, 0x1111, 960 * 1000);
    auto second = tone_stream(tx, "a", 100, 0x2222, 960 * 1000 - 48000, 7, {}, 100 * 20);
    arrivals.insert(arrivals.end(), second.begin(), second.end());
    int64_t first_audible_after_switch = -1;
    const int64_t switch_ms = 100 * 20 + 20;
    play(playout, std::move(arrivals), 200 * 20 + 300, [&](int64_t t, const int16_t* out, int) {
        if (t > switch_ms + 40 && first_audible_after_switch < 0 &&
            rms(out, VoicePlayout::kFrameSamples, 2) > 1000.0) {
            first_audible_after_switch = t;
        }
    });
    const auto s = playout.stats();
    EXPECT_EQ(s.stream_resets, 1u);
    ASSERT_GE(first_audible_after_switch, 0);
    EXPECT_LE(first_audible_after_switch - switch_ms, 200) << "the new stream must play at once";
}

// A sender restart on the P2P path keeps SSRC 0 and restarts the timestamp
// at the capture clock origin. A far-behind timestamp starts a new stream.
TEST(VoicePlayout, SenderRestartStartsANewStream) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    auto arrivals = tone_stream(tx, "p2p", 300, 0, 48000 * 60);
    auto restarted = tone_stream(tx, "p2p", 100, 0, 0, 0, {}, 300 * 20);
    arrivals.insert(arrivals.end(), restarted.begin(), restarted.end());
    int64_t first_audible_after_restart = -1;
    const int64_t restart_ms = 300 * 20 + 20;
    play(playout, std::move(arrivals), 400 * 20 + 300, [&](int64_t t, const int16_t* out, int) {
        if (t > restart_ms + 40 && first_audible_after_restart < 0 &&
            rms(out, VoicePlayout::kFrameSamples, 2) > 1000.0) {
            first_audible_after_restart = t;
        }
    });
    EXPECT_EQ(playout.stats().stream_resets, 1u);
    ASSERT_GE(first_audible_after_restart, 0) << "the restarted sender stayed silent";
    EXPECT_LE(first_audible_after_restart - restart_ms, 200);
}

// The mello sender's speech gate: nothing is sent while it is closed. When
// it opens at frame g, the pre-roll frames g-preroll..g-1 and frame g leave
// at once, when frame g is complete; later frames leave as they complete.
// Each spurt has `speech` frames after the gate opens and `gap` frames of
// silence before the next pre-roll. 20 ms one-way delay. One spurt per
// entry of `prerolls`, with that many pre-roll frames.
std::vector<Arrival> gated_stream(ToneSender& tx, const std::vector<uint32_t>& prerolls,
                                  uint32_t speech, uint32_t gap) {
    std::vector<Arrival> out;
    uint16_t seq = 500;
    uint32_t frame = 0;
    for (const uint32_t preroll : prerolls) {
        frame += gap;
        const uint32_t open = frame + preroll;
        for (uint32_t i = frame; i <= open + speech; ++i) {
            const uint32_t sent_after = std::max(i, open);  // the pre-roll waits for the gate
            Arrival a;
            a.arrival_ms = static_cast<int64_t>(sent_after + 1) * 20 + 20;
            a.peer = "gated";
            a.rtp.payload_type = 111;
            a.rtp.sequence = seq++;
            a.rtp.timestamp = 7777 + i * 960;
            a.rtp.ssrc = 0x6A7E;
            a.payload = tx.frame(i);
            out.push_back(std::move(a));
        }
        frame = open + speech + 1;
    }
    return out;
}

// The pre-roll burst at each talk-spurt start is sender behaviour, not
// network jitter (spec 10 section 5.2). NetEQ must not learn it as jitter
// and must not time-compress the speech to remove it: each spurt plays at
// its own delay. Upstream NetEQ alone raised its target delay above 100 ms
// and accelerated hundreds of ms of speech in the voice gate.
TEST(VoicePlayout, PreRollBurstIsHeldNotLearnedOrCompressed) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    // 6 spurts of 3 s with 100 ms of pre-roll, 1 s apart, then one spurt
    // with no pre-roll: it plays with no hold, at NetEQ's own target.
    const auto arrivals = gated_stream(tx, {5, 5, 5, 5, 5, 5, 0}, 150, 50);
    const int64_t last_spurt_ms = arrivals[arrivals.size() - 151].arrival_ms;
    const int64_t end_ms = arrivals.back().arrival_ms + 500;
    float max_hold = 0.0f;
    int max_target_last_spurt = 0;
    uint64_t accelerated_before_last = 0;
    play(playout, arrivals, end_ms, [&](int64_t t, const int16_t*, int) {
        if (t % 100 != 0) return;
        const auto s = playout.stats();
        max_hold = std::max(max_hold, s.spurt_hold_ms);
        if (t < last_spurt_ms) accelerated_before_last = s.removed_samples_for_acceleration;
        if (t > last_spurt_ms + 400 && t < end_ms - 600) {
            max_target_last_spurt = std::max(max_target_last_spurt, static_cast<int>(s.target_delay_ms));
        }
    });
    const auto s = playout.stats();
    EXPECT_EQ(s.talk_spurts, 7u);
    EXPECT_GE(max_hold, 80.0f) << "the pre-roll lateness must be held";
    EXPECT_LE(max_hold, 140.0f);
    EXPECT_EQ(s.spurt_hold_ms, 0.0f) << "a spurt with no pre-roll holds nothing";
    // The six pre-roll spurts are not compressed: at most the start-up of
    // the first one (about 30 ms) in 18 s of speech.
    EXPECT_LE(accelerated_before_last, 48u * 40u)
        << "accelerated " << accelerated_before_last / 48 << " ms";
    EXPECT_LE(max_target_last_spurt, 40) << "the pre-roll was learned as network jitter";
    EXPECT_EQ(s.insert_errors, 0u);
}

// A stream without gate gaps (push-to-talk) is one spurt: NetEQ behaves as
// upstream, and real network jitter still raises the target.
TEST(VoicePlayout, NetworkJitterStillRaisesTheTarget) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    auto arrivals = tone_stream(tx, "a", 500);
    // Every 7th packet is held back 80 ms (one network path, so the next
    // ones queue behind it).
    int64_t last = 0;
    for (auto& a : arrivals) {
        if (a.rtp.sequence % 7 == 0) a.arrival_ms += 80;
        a.arrival_ms = std::max(a.arrival_ms, last);
        last = a.arrival_ms;
    }
    int max_target = 0;
    play(playout, std::move(arrivals), 500 * 20 + 200, [&](int64_t t, const int16_t*, int) {
        if (t % 100 == 0) max_target = std::max(max_target, static_cast<int>(playout.stats().target_delay_ms));
    });
    const auto s = playout.stats();
    EXPECT_EQ(s.talk_spurts, 1u);
    EXPECT_EQ(s.spurt_hold_ms, 0.0f);
    EXPECT_GE(max_target, 60) << "NetEQ must still learn real jitter";
}

// A target jump (here: reordered packets make the delay manager raise its
// target in one step) does not stretch the speech to fill the buffer up to
// the new target (preemptive expand) for 5 s. Upstream NetEQ stretches at
// once, and compresses again when the target decays.
TEST(VoicePlayout, TargetJumpDoesNotStretchSpeech) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    auto arrivals = tone_stream(tx, "a", 400);
    for (auto& a : arrivals) {
        // From 3 s, every 10th packet arrives 70 ms late, after the next ones.
        if (a.arrival_ms >= 3000 && a.arrival_ms < 6000 && a.rtp.sequence % 10 == 0) {
            a.arrival_ms += 70;
        }
    }
    int max_target = 0;
    uint64_t stretched_before_jump = 0;
    play(playout, std::move(arrivals), 6000, [&](int64_t t, const int16_t*, int) {
        const auto q = playout.stats();
        if (t <= 3000) stretched_before_jump = q.inserted_samples_for_deceleration;
        max_target = std::max(max_target, static_cast<int>(q.target_delay_ms));
    });
    const auto s = playout.stats();
    EXPECT_GE(max_target, 60) << "the target did not jump; the test proves nothing";
    // NetEQ's own fill at stream start (target 80 ms) stays; after the jump
    // nothing is stretched.
    const uint64_t stretched = s.inserted_samples_for_deceleration - stretched_before_jump;
    EXPECT_LE(stretched, 48u * 5u) << "stretched " << stretched / 48 << " ms after the jump";
}

// Reordered packets a little behind the newest timestamp are not a restart.
TEST(VoicePlayout, ReorderIsNotARestart) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    auto arrivals = tone_stream(tx, "a", 200);
    for (auto& a : arrivals) {
        if (a.rtp.sequence % 10 == 3) a.arrival_ms += 45;  // arrives after the next two
    }
    play(playout, std::move(arrivals), 200 * 20 + 300, [](int64_t, const int16_t*, int) {});
    const auto s = playout.stats();
    EXPECT_EQ(s.stream_resets, 0u);
    EXPECT_EQ(s.insert_errors, 0u);
}

// Late audio reaches the health counter: a 200 ms stall holds packets 50..59
// and they arrive together. NetEQ expands while they wait.
TEST(VoicePlayout, StallWithPacketsWaitingCountsLateFrames) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    auto arrivals = tone_stream(tx, "a", 150);
    for (auto& a : arrivals) {
        if (a.arrival_ms >= 1020 && a.arrival_ms <= 1200) a.arrival_ms = 1220;
    }
    // A loss inside the stall, so expand runs with packets in the buffer.
    arrivals.erase(std::remove_if(arrivals.begin(), arrivals.end(),
                                  [](const Arrival& a) { return a.rtp.sequence == 100 + 52; }),
                   arrivals.end());
    play(playout, std::move(arrivals), 150 * 20 + 300, [](int64_t, const int16_t*, int) {});
    EXPECT_GT(playout.stats().late_frames, 0u);
}

// A talk pause is not late audio: nothing waits in the buffer while NetEQ
// expands into the pause.
TEST(VoicePlayout, TalkPauseIsNotLate) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    play(playout, tone_stream(tx, "a", 50), 50 * 20 + 3000, [](int64_t, const int16_t*, int) {});
    const auto s = playout.stats();
    EXPECT_GT(s.concealed_samples, 0u) << "NetEQ must have expanded into the pause";
    EXPECT_EQ(s.late_frames, 0u);
}

// A long gate gap (a minute with no packet, timestamps advance) leaves no
// delay behind: speech after it plays at the delay it had before.
TEST(VoicePlayout, LongGapLeavesNoDelay) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    auto arrivals = tone_stream(tx, "a", 200, 0x77, 1000);
    auto after = tone_stream(tx, "a", 200, 0x77, 1000 + (200 + 3000) * 960, 300, {},
                             (200 + 3000) * 20);
    arrivals.insert(arrivals.end(), after.begin(), after.end());
    float before_ms = -1.0f;
    play(playout, std::move(arrivals), (200 + 3000 + 200) * 20 + 200,
         [&](int64_t t, const int16_t*, int) {
             if (t == 3900) before_ms = playout.stats().filtered_delay_ms;
         });
    const auto s = playout.stats();
    ASSERT_GE(before_ms, 0.0f);
    EXPECT_LE(s.filtered_delay_ms, before_ms + 20.0f) << "delay built up over the gap";
    EXPECT_EQ(s.insert_errors, 0u);
}

// ---------------------------------------------------------------------------
// Mixer: volume, pan, two peers, lifetime of NetEQ instances
// ---------------------------------------------------------------------------

// Volume set before the peer's first packet applies to its stream. Pan
// moves the peer to one channel; centre is unity on both.
TEST(VoicePlayout, VolumeAndPanApplyPerPeer) {
    struct Levels {
        double left = 0.0, right = 0.0;
        int frames = 0;
    };
    auto run = [&](float vol, float pan) {
        VoicePlayout playout(default_config());
        playout.set_peer_volume("a", vol);
        playout.set_peer_pan("a", pan);
        Levels lv;
        ToneSender tx;
        play(playout, tone_stream(tx, "a", 150), 150 * 20 + 100,
             [&](int64_t t, const int16_t* out, int) {
                 if (t < 1000 || t > 2800) return;
                 lv.left += rms(out, VoicePlayout::kFrameSamples, 2, 0);
                 lv.right += rms(out, VoicePlayout::kFrameSamples, 2, 1);
                 ++lv.frames;
             });
        lv.left /= lv.frames;
        lv.right /= lv.frames;
        EXPECT_FLOAT_EQ(playout.peer_volume("a"), std::clamp(vol, 0.0f, 2.0f));
        return lv;
    };
    const Levels centre = run(1.0f, 0.0f);
    ASSERT_GT(centre.left, 3000.0);
    EXPECT_NEAR(centre.left, centre.right, 1.0) << "centre must be the same on both channels";

    const Levels left = run(1.0f, -1.0f);
    EXPECT_NEAR(left.left, centre.left, centre.left * 0.02);
    EXPECT_LT(left.right, 1.0) << "pan -1 must silence the right channel";

    const Levels half = run(0.5f, 0.5f);
    EXPECT_NEAR(half.right, centre.right * 0.5, centre.right * 0.03);
    EXPECT_NEAR(half.left, centre.left * 0.25, centre.left * 0.03);

    const Levels muted = run(0.0f, 0.0f);
    EXPECT_LT(muted.left + muted.right, 1.0);
}

// Two peers mix through the limiter; each has its own NetEQ. Removing one
// destroys its NetEQ and keeps its counts in the lifetime totals.
TEST(VoicePlayout, TwoPeersMixAndRemoveOne) {
    VoicePlayout playout(default_config());
    ToneSender tx_a(300.0), tx_b(440.0);
    auto arrivals = tone_stream(tx_a, "a", 150, 0xA);
    auto b = tone_stream(tx_b, "b", 150, 0xB, 777, 9);
    arrivals.insert(arrivals.end(), b.begin(), b.end());
    int max_sources = 0;
    play(playout, std::move(arrivals), 150 * 20 + 100,
         [&](int64_t, const int16_t*, int sources) { max_sources = std::max(max_sources, sources); });
    EXPECT_EQ(max_sources, 2);
    EXPECT_EQ(playout.peer_count(), 2);
    const uint64_t received_before = playout.stats().total_samples_received;

    playout.remove_peer("b");
    EXPECT_EQ(playout.peer_count(), 1);
    EXPECT_EQ(playout.stats().total_samples_received, received_before)
        << "a removed peer's counts stay in the lifetime totals";
    EXPECT_EQ(playout.clear(), 1u);
    EXPECT_EQ(playout.peer_count(), 0);
    std::vector<int16_t> out(kStereoSamples, 123);
    EXPECT_EQ(playout.mix(out.data()), 0);
    EXPECT_EQ(rms(out.data(), out.size()), 0.0);
}

TEST(VoicePlayout, RejectsOtherPayloadTypesAndJunk) {
    VoicePlayout playout(default_config());
    ToneSender tx;
    const auto payload = tx.frame(0);
    const auto video = make_rtp(1, 960, 5, payload, 96);
    EXPECT_FALSE(playout.insert_rtp("a", video.data(), video.size(), 0));
    const std::vector<uint8_t> junk{1, 2, 3, 4, 5};
    EXPECT_FALSE(playout.insert_rtp("a", junk.data(), junk.size(), 0));
    const auto opus = make_rtp(1, 960, 5, payload);
    EXPECT_TRUE(playout.insert_rtp("a", opus.data(), opus.size(), 0));
    const auto s = playout.stats();
    EXPECT_EQ(s.packets_rejected, 2u);
    EXPECT_EQ(s.packets_inserted, 1u);
    EXPECT_EQ(s.peers, 1);
}

// ---------------------------------------------------------------------------
// Sinc resampler (WASAPI device edges)
// ---------------------------------------------------------------------------

namespace {

// Resample a 1 kHz tone from `in_rate` to `out_rate` in uneven chunks and
// return the output after the filter has settled.
std::vector<float> resample_tone(int in_rate, int out_rate, int channels) {
    StreamResampler rs(in_rate, out_rate, channels);
    std::vector<float> out;
    const size_t total_in = static_cast<size_t>(in_rate);  // 1 s
    std::vector<float> chunk;
    size_t pos = 0;
    size_t step = 37;
    while (pos < total_in) {
        const size_t n = std::min(step, total_in - pos);
        chunk.resize(n * channels);
        for (size_t i = 0; i < n; ++i) {
            const float v = 0.5f * static_cast<float>(std::sin(2.0 * kPi * 1000.0 * (pos + i) / in_rate));
            for (int c = 0; c < channels; ++c) chunk[i * channels + c] = c == 0 ? v : -v;
        }
        rs.push(chunk.data(), n);
        std::vector<float> got(rs.available() * channels);
        rs.pull(got.data(), rs.available());
        out.insert(out.end(), got.begin(), got.end());
        pos += n;
        step = step == 37 ? 1013 : 37;  // device callbacks vary in size
    }
    return out;
}

}  // namespace

TEST(StreamResampler, ConvertsRateAndKeepsTheTone) {
    for (int in_rate : {44100, 96000, 16000, 22050}) {
        const auto out = resample_tone(in_rate, 48000, 2);
        const size_t frames = out.size() / 2;
        // 1 s in, about 1 s out (less at most one block held back).
        EXPECT_NEAR(static_cast<double>(frames), 48000.0, 1000.0) << in_rate;
        // Skip the filter warm-up, then check level and frequency on the
        // left channel, and that the right channel is its inverse.
        const size_t from = 2400;
        double sum = 0.0;
        int crossings = 0;
        for (size_t i = from; i < frames; ++i) {
            const float l = out[i * 2];
            sum += static_cast<double>(l) * l;
            EXPECT_NEAR(out[i * 2 + 1], -l, 1e-4) << in_rate << " channel mix-up at " << i;
            if (i > from && (out[(i - 1) * 2] < 0.0f) != (l < 0.0f)) ++crossings;
        }
        const double level = std::sqrt(sum / static_cast<double>(frames - from));
        EXPECT_NEAR(level, 0.5 / std::sqrt(2.0), 0.01) << in_rate;
        const double hz = crossings / 2.0 / (static_cast<double>(frames - from) / 48000.0);
        EXPECT_NEAR(hz, 1000.0, 5.0) << in_rate;
    }
}

TEST(StreamResampler, PlaybackDirectionUsesTenMillisecondBlocks) {
    StreamResampler rs(48000, 44100, 2);
    EXPECT_EQ(rs.in_block_frames(), 480u);
    EXPECT_EQ(rs.out_block_frames(), 441u);
    StreamResampler odd(22050, 48000, 1);
    EXPECT_GE(odd.in_block_frames(), 220u);
    EXPECT_EQ(odd.in_block_frames() * 48000 % 22050, 0u);
    EXPECT_EQ(odd.in_block_frames() * 48000 / 22050, odd.out_block_frames());
}
