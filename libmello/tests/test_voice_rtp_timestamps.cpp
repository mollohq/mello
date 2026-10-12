// The voice RTP track carries the frame's media time as its RTP timestamp.
//
// A libmello voice peer (the SFU leg) sends frames to a plain libdatachannel
// peer that reads the RTP headers on the wire. libdatachannel only updates the
// RTP timestamp from a FrameInfo, so a plain track->send() leaves every packet
// at the start timestamp.

#include <gtest/gtest.h>

#include "mello.h"

#include <rtc/rtc.hpp>

#include <chrono>
#include <cstdint>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <tuple>
#include <vector>

namespace {

using namespace std::chrono_literals;

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
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (std::chrono::steady_clock::now() < deadline) {
        if (predicate()) return true;
        std::this_thread::sleep_for(5ms);
    }
    return predicate();
}

uint32_t read_be32(const uint8_t* p) {
    return (static_cast<uint32_t>(p[0]) << 24) | (static_cast<uint32_t>(p[1]) << 16) |
           (static_cast<uint32_t>(p[2]) << 8) | static_cast<uint32_t>(p[3]);
}

// Frame index (first payload byte) -> RTP timestamp, as received.
struct Received {
    std::mutex mutex;
    std::map<int, uint32_t> timestamps;
};

void record_rtp(Received& received, const rtc::binary& bin) {
    if (bin.size() < 13) return;
    const auto* b = reinterpret_cast<const uint8_t*>(bin.data());
    if ((b[1] & 0x7F) != 111) return;  // Opus only; RTCP has other types
    size_t header = 12 + static_cast<size_t>(b[0] & 0x0F) * 4;
    if ((b[0] & 0x10) != 0 && header + 4 <= bin.size()) {
        const size_t ext_words = (static_cast<size_t>(b[header + 2]) << 8) | b[header + 3];
        header += 4 + ext_words * 4;
    }
    if (header >= bin.size()) return;
    std::lock_guard<std::mutex> lock(received.mutex);
    received.timestamps.emplace(static_cast<int>(b[header]), read_be32(b + 4));
}

TEST(VoiceRtpTimestamps, FramesCarryTheirMediaTimeOnTheWire) {
    CandidateQueue voice_candidates;
    Received received;

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
        track->onMessage(
            [&received](rtc::binary bin) { record_rtp(received, bin); },
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

    // The send track opens shortly after the connection. A send before that
    // is skipped, so wait until a probe frame goes out.
    const uint8_t probe[2] = {0xFF, 0};
    ASSERT_TRUE(wait_until(
        [&]() { return mello_peer_send_audio_frame(voice, probe, 2, 0) == MELLO_OK; }, 5s));

    // Frames 1..4 at media times 960 apart, then frame 5 after a 1 s gate
    // gap (48000 samples), as the capture clock produces them.
    const uint32_t media[] = {0, 960, 1920, 2880, 2880 + 960 + 48000};
    for (int i = 0; i < 5; ++i) {
        const uint8_t frame[2] = {static_cast<uint8_t>(i + 1), 0};
        ASSERT_EQ(mello_peer_send_audio_frame(voice, frame, 2, 100000 + media[i]), MELLO_OK);
        std::this_thread::sleep_for(20ms);
    }
    ASSERT_TRUE(wait_until(
        [&]() {
            std::lock_guard<std::mutex> lock(received.mutex);
            int got = 0;
            for (int i = 1; i <= 5; ++i) got += received.timestamps.count(i) ? 1 : 0;
            return got == 5;
        },
        5s));

    {
        std::lock_guard<std::mutex> lock(received.mutex);
        const uint32_t base = received.timestamps.at(1);
        for (int i = 2; i <= 5; ++i) {
            EXPECT_EQ(received.timestamps.at(i) - base, media[i - 1] - media[0]) << "frame " << i;
        }
        EXPECT_EQ(received.timestamps.at(2) - received.timestamps.at(1), 960u);
        EXPECT_EQ(received.timestamps.at(5) - received.timestamps.at(4), 48000u + 960u);
    }

    mello_peer_destroy(voice);
    remote->close();
    {
        std::lock_guard<std::mutex> lock(tracks_mutex);
        tracks.clear();
    }
    remote.reset();
}

}  // namespace
