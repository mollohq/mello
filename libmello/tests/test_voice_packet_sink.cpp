// Voice packet sink on the wire (spec 10 section 4.5).
//
// A real AudioPipeline on the device-free backend (MELLO_AUDIO_BACKEND=test)
// sends each encoded frame from the capture thread through the packet sink
// into a libmello voice peer (the SFU leg). A plain libdatachannel peer reads
// the RTP packets. No queue and no poll loop sit between encode and send.

#include <gtest/gtest.h>

#include "audio/audio_pipeline.hpp"
#include "audio/opus_codec.hpp"
#include "mello.h"

#include <rtc/rtc.hpp>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#endif

namespace {

using namespace std::chrono_literals;
using Clock = std::chrono::steady_clock;
using namespace mello::audio;

void select_test_backend() {
#ifdef _WIN32
    SetEnvironmentVariableA("MELLO_AUDIO_BACKEND", "test");
#else
    setenv("MELLO_AUDIO_BACKEND", "test", 1);
#endif
}

struct CandidateQueue {
    std::mutex mutex;
    std::vector<std::pair<std::string, std::string>> items;  // candidate, mid
};

void on_voice_candidate(void* user_data, const MelloIceCandidate* candidate) {
    if (!user_data || !candidate || !candidate->candidate) return;
    auto* queue = static_cast<CandidateQueue*>(user_data);
    std::lock_guard<std::mutex> lock(queue->mutex);
    queue->items.emplace_back(candidate->candidate,
                              candidate->sdp_mid ? candidate->sdp_mid : "");
}

template <typename Predicate>
bool wait_until(Predicate predicate, std::chrono::milliseconds timeout) {
    const auto deadline = Clock::now() + timeout;
    while (Clock::now() < deadline) {
        if (predicate()) return true;
        std::this_thread::sleep_for(5ms);
    }
    return predicate();
}

uint32_t read_be32(const uint8_t* p) {
    return (static_cast<uint32_t>(p[0]) << 24) | (static_cast<uint32_t>(p[1]) << 16) |
           (static_cast<uint32_t>(p[2]) << 8) | static_cast<uint32_t>(p[3]);
}

// Opus RTP packets as received: RTP timestamp and arrival time.
struct Arrivals {
    std::mutex mutex;
    std::vector<std::pair<uint32_t, Clock::time_point>> packets;
};

void record_rtp(Arrivals& arrivals, const rtc::binary& bin) {
    const auto now = Clock::now();
    if (bin.size() < 13) return;
    const auto* b = reinterpret_cast<const uint8_t*>(bin.data());
    if ((b[1] & 0x7F) != 111) return;  // Opus only; RTCP has other types
    std::lock_guard<std::mutex> lock(arrivals.mutex);
    arrivals.packets.emplace_back(read_be32(b + 4), now);
}

void send_to_peer(MelloPeerConnection* peer, const uint8_t* data, int size, uint32_t ts) {
    mello_peer_send_audio_frame(peer, data, size, ts);
}

TEST(VoicePacketSinkWire, CapturedFramesReachAPlainReceiverOnTheCaptureThread) {
    select_test_backend();
    AudioPipeline pipeline;
    ASSERT_TRUE(pipeline.initialize());
    pipeline.set_push_to_talk(true);  // every unmuted frame is encoded
    ASSERT_TRUE(pipeline.start_capture_inject());

    CandidateQueue voice_candidates;
    Arrivals arrivals;

    MelloPeerConnection* voice = mello_peer_create(nullptr, "voice-peer");
    ASSERT_NE(voice, nullptr);
    mello_peer_set_ice_servers(voice, nullptr, 0);
    mello_peer_set_ice_callback(voice, on_voice_candidate, &voice_candidates);
    const char* offer_ptr = mello_peer_create_offer(voice);
    ASSERT_NE(offer_ptr, nullptr);
    const std::string offer(offer_ptr);

    auto remote = std::make_shared<rtc::PeerConnection>(rtc::Configuration{});
    std::mutex answer_mutex;
    std::string answer;
    remote->onLocalDescription([&](rtc::Description description) {
        std::lock_guard<std::mutex> lock(answer_mutex);
        answer = std::string(description);
    });
    remote->onLocalCandidate([voice](rtc::Candidate candidate) {
        const std::string text = candidate.candidate();
        const std::string mid = candidate.mid();
        MelloIceCandidate c{};
        c.candidate = text.c_str();
        c.sdp_mid = mid.c_str();
        c.sdp_mline_index = 0;
        mello_peer_add_ice_candidate(voice, &c);
    });
    std::vector<std::shared_ptr<rtc::Track>> tracks;
    std::mutex tracks_mutex;
    remote->onTrack([&](std::shared_ptr<rtc::Track> track) {
        if (track->description().type() != "audio") return;
        track->setMediaHandler(std::make_shared<rtc::RtcpReceivingSession>());
        track->onMessage([&arrivals](rtc::binary bin) { record_rtp(arrivals, bin); },
                         [](rtc::string) {});
        std::lock_guard<std::mutex> lock(tracks_mutex);
        tracks.push_back(std::move(track));
    });

    remote->setRemoteDescription(rtc::Description(offer, rtc::Description::Type::Offer));
    ASSERT_TRUE(wait_until(
        [&]() {
            std::lock_guard<std::mutex> lock(answer_mutex);
            return !answer.empty();
        },
        5s));
    {
        std::lock_guard<std::mutex> lock(answer_mutex);
        ASSERT_EQ(mello_peer_set_remote_description(voice, answer.c_str(), false), MELLO_OK);
    }
    ASSERT_TRUE(wait_until(
        [&]() {
            std::vector<std::pair<std::string, std::string>> pending;
            {
                std::lock_guard<std::mutex> lock(voice_candidates.mutex);
                pending.swap(voice_candidates.items);
            }
            for (const auto& [candidate, mid] : pending) {
                remote->addRemoteCandidate(rtc::Candidate(candidate, mid));
            }
            return mello_peer_is_connected(voice) &&
                   remote->state() == rtc::PeerConnection::State::Connected;
        },
        10s));

    // The send track opens shortly after the connection. Probe until one
    // frame goes out. Its media time marks the RTP start timestamp.
    const uint32_t kProbeMedia = 0x40000000u;
    const uint8_t probe[2] = {0xFF, 0};
    ASSERT_TRUE(wait_until(
        [&]() { return mello_peer_send_audio_frame(voice, probe, 2, kProbeMedia) == MELLO_OK; },
        5s));
    ASSERT_TRUE(wait_until(
        [&]() {
            std::lock_guard<std::mutex> lock(arrivals.mutex);
            return !arrivals.packets.empty();
        },
        5s));
    uint32_t rtp_start = 0;
    {
        std::lock_guard<std::mutex> lock(arrivals.mutex);
        rtp_start = arrivals.packets.front().first - kProbeMedia;
        arrivals.packets.clear();
    }

    pipeline.set_packet_sink([voice](const uint8_t* data, int size, uint32_t ts, uint32_t) {
        send_to_peer(voice, data, size, ts);
    });

    // Frames of a 300 Hz tone, 20 ms apart, injected in 10 ms chunks like a
    // device callback. Each frame is encoded and sent inside the second
    // inject call of its pair.
    constexpr int kFrames = 100;
    std::vector<Clock::time_point> encoded_by(kFrames);
    std::vector<int16_t> chunk(FRAME_SIZE / 2);
    size_t sample = 0;
    for (int f = 0; f < kFrames; ++f) {
        for (int half = 0; half < 2; ++half) {
            for (auto& s : chunk) {
                s = static_cast<int16_t>(
                    6000.0 * std::sin(2.0 * 3.14159265358979 * 300.0 *
                                      static_cast<double>(sample++) / SAMPLE_RATE));
            }
            if (half == 1) encoded_by[f] = Clock::now();
            pipeline.inject_capture(chunk.data(), static_cast<int>(chunk.size()));
        }
        std::this_thread::sleep_for(20ms);
    }

    ASSERT_TRUE(wait_until(
        [&]() {
            std::lock_guard<std::mutex> lock(arrivals.mutex);
            return arrivals.packets.size() >= static_cast<size_t>(kFrames);
        },
        5s));

    uint8_t buf[MAX_PACKET_SIZE + 4];
    EXPECT_EQ(pipeline.get_packet(buf, sizeof(buf)), 0) << "a sent frame was also queued";

    // Each frame arrives once, with its media time on the wire.
    std::map<uint32_t, Clock::time_point> by_frame;
    {
        std::lock_guard<std::mutex> lock(arrivals.mutex);
        for (const auto& [rtp, at] : arrivals.packets) {
            const uint32_t media = rtp - rtp_start;
            ASSERT_EQ(media % FRAME_SIZE, 0u) << "media time " << media;
            by_frame.emplace(media / FRAME_SIZE, at);
        }
    }
    ASSERT_EQ(by_frame.size(), static_cast<size_t>(kFrames));
    std::vector<double> latency_ms;
    for (int f = 0; f < kFrames; ++f) {
        const auto it = by_frame.find(static_cast<uint32_t>(f));
        ASSERT_NE(it, by_frame.end()) << "frame " << f << " missing";
        latency_ms.push_back(
            std::chrono::duration<double, std::milli>(it->second - encoded_by[f]).count());
    }
    std::sort(latency_ms.begin(), latency_ms.end());
    const auto pct = [&](double p) {
        return latency_ms[static_cast<size_t>(p * (latency_ms.size() - 1))];
    };
    // Informational: capture (last chunk of the frame) to the plain receiver,
    // including DSP, encode and the loopback network. Not a gate here; the
    // gate runs in mello-core with the command loop blocked.
    std::printf("[sink-wire] frames=%d capture-to-wire ms p50=%.3f p95=%.3f p99=%.3f max=%.3f\n",
                kFrames, pct(0.50), pct(0.95), pct(0.99), latency_ms.back());

    pipeline.set_packet_sink(nullptr);
    pipeline.shutdown();
    mello_peer_destroy(voice);
    remote->close();
    {
        std::lock_guard<std::mutex> lock(tracks_mutex);
        tracks.clear();
    }
    remote.reset();
}

}  // namespace
