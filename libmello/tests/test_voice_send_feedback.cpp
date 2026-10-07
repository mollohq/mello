// Receiver reports about the voice send track (spec 10 section 7.5).
//
// The parser and the tracker get crafted RTCP. The wire test sends voice
// frames from a libmello voice peer to a plain libdatachannel peer that
// answers each of our sender reports with a crafted receiver report, as the
// SFU's Pion receiver interceptor does once per second.

#include <gtest/gtest.h>

#include "mello.h"
#include "transport/voice_send_feedback.hpp"

#include <rtc/rtc.hpp>

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <utility>
#include <vector>

using mello::transport::find_reception_reports;
using mello::transport::ReceptionReport;
using mello::transport::rtt_ms_from_report;
using mello::transport::SendFeedbackTracker;

namespace {

using namespace std::chrono_literals;

void put_be16(std::vector<uint8_t>& out, uint16_t v) {
    out.push_back(static_cast<uint8_t>(v >> 8));
    out.push_back(static_cast<uint8_t>(v));
}

void put_be32(std::vector<uint8_t>& out, uint32_t v) {
    out.push_back(static_cast<uint8_t>(v >> 24));
    out.push_back(static_cast<uint8_t>(v >> 16));
    out.push_back(static_cast<uint8_t>(v >> 8));
    out.push_back(static_cast<uint8_t>(v));
}

uint32_t read_be32(const uint8_t* p) {
    return (static_cast<uint32_t>(p[0]) << 24) | (static_cast<uint32_t>(p[1]) << 16) |
           (static_cast<uint32_t>(p[2]) << 8) | static_cast<uint32_t>(p[3]);
}

void put_block(std::vector<uint8_t>& out, const ReceptionReport& b) {
    put_be32(out, b.source_ssrc);
    const uint32_t lost24 = static_cast<uint32_t>(b.cumulative_lost) & 0x00FFFFFFu;
    put_be32(out, (static_cast<uint32_t>(b.fraction_lost) << 24) | lost24);
    put_be32(out, b.extended_highest_seq);
    put_be32(out, b.jitter);
    put_be32(out, b.last_sr);
    put_be32(out, b.delay_since_last_sr);
}

// RTCP receiver report (PT 201) from `reporter` with `blocks`.
std::vector<uint8_t> receiver_report(uint32_t reporter, const std::vector<ReceptionReport>& blocks) {
    std::vector<uint8_t> out;
    out.push_back(static_cast<uint8_t>(0x80 | blocks.size()));
    out.push_back(201);
    put_be16(out, static_cast<uint16_t>(1 + 6 * blocks.size()));
    put_be32(out, reporter);
    for (const auto& b : blocks) put_block(out, b);
    return out;
}

// RTCP sender report (PT 200) from `sender` with `blocks`.
std::vector<uint8_t> sender_report(uint32_t sender, const std::vector<ReceptionReport>& blocks) {
    std::vector<uint8_t> out;
    out.push_back(static_cast<uint8_t>(0x80 | blocks.size()));
    out.push_back(200);
    put_be16(out, static_cast<uint16_t>(6 + 6 * blocks.size()));
    put_be32(out, sender);
    put_be32(out, 0xE1234567u);  // NTP seconds
    put_be32(out, 0x89ABCDEFu);  // NTP fraction
    put_be32(out, 123456u);      // RTP timestamp
    put_be32(out, 10u);          // packet count
    put_be32(out, 1000u);        // octet count
    for (const auto& b : blocks) put_block(out, b);
    return out;
}

// RTCP SDES (PT 202) with one CNAME chunk, padded to 32 bits.
std::vector<uint8_t> sdes(uint32_t ssrc) {
    std::vector<uint8_t> out{0x81, 202, 0, 3};
    put_be32(out, ssrc);
    out.insert(out.end(), {1, 4, 'm', 'e', 'l', 'o', 0, 0});
    return out;
}

constexpr uint32_t kOurSsrc = 0x5EED0001u;

ReceptionReport block_about(uint32_t ssrc) {
    ReceptionReport b;
    b.source_ssrc = ssrc;
    b.fraction_lost = 64;  // 25 %
    b.cumulative_lost = 16;
    b.extended_highest_seq = 0x00010005u;
    b.jitter = 480;  // 10 ms at 48 kHz
    b.last_sr = 0x12345678u;
    b.delay_since_last_sr = 0x8000u;  // 0.5 s
    return b;
}

TEST(VoiceSendFeedback, ParsesTheReceiverReportBlockAboutTheSendTrack) {
    const auto other = block_about(0x0BADF00Du);
    const auto ours = block_about(kOurSsrc);
    const auto rr = receiver_report(0xAAAA0001u, {other, ours});

    const auto got = find_reception_reports(rr.data(), rr.size(), kOurSsrc);
    ASSERT_EQ(got.size(), 1u);
    EXPECT_EQ(got[0].reporter_ssrc, 0xAAAA0001u);
    EXPECT_EQ(got[0].source_ssrc, kOurSsrc);
    EXPECT_EQ(got[0].fraction_lost, 64);
    EXPECT_EQ(got[0].cumulative_lost, 16);
    EXPECT_EQ(got[0].extended_highest_seq, 0x00010005u);
    EXPECT_EQ(got[0].jitter, 480u);
    EXPECT_EQ(got[0].last_sr, 0x12345678u);
    EXPECT_EQ(got[0].delay_since_last_sr, 0x8000u);

    EXPECT_TRUE(find_reception_reports(rr.data(), rr.size(), 0x01020304u).empty());
}

TEST(VoiceSendFeedback, CumulativeLostIsSigned24Bit) {
    auto b = block_about(kOurSsrc);
    b.cumulative_lost = -2;  // duplicates make it negative (RFC 3550 6.4.1)
    const auto rr = receiver_report(1, {b});
    const auto got = find_reception_reports(rr.data(), rr.size(), kOurSsrc);
    ASSERT_EQ(got.size(), 1u);
    EXPECT_EQ(got[0].cumulative_lost, -2);
}

TEST(VoiceSendFeedback, FindsBlocksInACompoundPacketAndStopsAtAMalformedOne) {
    auto in_sr = block_about(kOurSsrc);
    in_sr.cumulative_lost = 3;
    auto in_rr = block_about(kOurSsrc);
    in_rr.cumulative_lost = 4;
    std::vector<uint8_t> compound = sender_report(0xBBBB0001u, {in_sr});
    const auto s = sdes(0xBBBB0001u);
    compound.insert(compound.end(), s.begin(), s.end());
    const auto rr = receiver_report(0xBBBB0001u, {in_rr});
    compound.insert(compound.end(), rr.begin(), rr.end());

    auto got = find_reception_reports(compound.data(), compound.size(), kOurSsrc);
    ASSERT_EQ(got.size(), 2u);
    EXPECT_EQ(got[0].cumulative_lost, 3);
    EXPECT_EQ(got[1].cumulative_lost, 4);

    // A length that runs past the end: nothing after it is read, no crash.
    std::vector<uint8_t> broken = sender_report(0xBBBB0001u, {in_sr});
    std::vector<uint8_t> bad_rr = receiver_report(1, {in_rr});
    bad_rr[3] = 40;
    broken.insert(broken.end(), bad_rr.begin(), bad_rr.end());
    got = find_reception_reports(broken.data(), broken.size(), kOurSsrc);
    ASSERT_EQ(got.size(), 1u);
    EXPECT_EQ(got[0].cumulative_lost, 3);

    // A report count that needs more blocks than the length holds.
    std::vector<uint8_t> short_rr = receiver_report(1, {in_rr});
    short_rr[0] = 0x82;
    EXPECT_TRUE(find_reception_reports(short_rr.data(), short_rr.size(), kOurSsrc).empty());

    // Not RTCP version 2.
    std::vector<uint8_t> v1 = receiver_report(1, {in_rr});
    v1[0] = 0x41;
    EXPECT_TRUE(find_reception_reports(v1.data(), v1.size(), kOurSsrc).empty());
    EXPECT_TRUE(find_reception_reports(nullptr, 0, kOurSsrc).empty());
}

TEST(VoiceSendFeedback, RoundTripIsArrivalMinusLsrMinusDlsr) {
    // Sender report at LSR, held 0.5 s by the reporter, 80 ms of network.
    const uint32_t lsr = 0x12345678u;
    const uint32_t dlsr = 0x8000u;
    const uint32_t arrival = lsr + dlsr + static_cast<uint32_t>(0.080 * 65536.0);
    EXPECT_NEAR(rtt_ms_from_report(lsr, dlsr, arrival), 80.0f, 0.1f);

    // Across the 32-bit wrap of the compact NTP time.
    const uint32_t lsr_wrap = 0xFFFFF000u;
    const uint32_t arrival_wrap = lsr_wrap + dlsr + static_cast<uint32_t>(0.150 * 65536.0);
    EXPECT_NEAR(rtt_ms_from_report(lsr_wrap, dlsr, arrival_wrap), 150.0f, 0.1f);

    // No sender report reached the reporter yet.
    EXPECT_EQ(rtt_ms_from_report(0, 0, arrival), -1.0f);
    // A report that claims to be from the future.
    EXPECT_EQ(rtt_ms_from_report(lsr, dlsr, lsr + dlsr - 100), -1.0f);
}

TEST(VoiceSendFeedback, TrackerGivesPacketsExpectedAndLostPerReport) {
    SendFeedbackTracker tracker;
    MelloSendFeedback fb{};
    EXPECT_FALSE(tracker.latest(&fb));
    tracker.set_first_sequence(65530);

    // Pion reports the stream before its first packet: sequence 0, nothing
    // lost. No interval, and the next report still counts from the start.
    ReceptionReport empty{};
    empty.source_ssrc = kOurSsrc;
    fb = tracker.on_report(empty, 0x40000000u, 48000);
    EXPECT_EQ(fb.reports, 1u);
    EXPECT_EQ(fb.interval_expected, 0u);

    // First report: the stream started at 65530, wrapped, and reached 19:
    // 26 packets expected, 2 lost.
    ReceptionReport r{};
    r.source_ssrc = kOurSsrc;
    r.extended_highest_seq = (1u << 16) | 19u;
    r.cumulative_lost = 2;
    r.fraction_lost = 20;
    r.jitter = 96;
    const uint32_t now = 0x40000000u;
    fb = tracker.on_report(r, now, 48000);
    EXPECT_EQ(fb.reports, 2u);
    EXPECT_EQ(fb.interval_expected, 26u);
    EXPECT_EQ(fb.interval_lost, 2u);
    EXPECT_NEAR(fb.fraction_lost, 20.0f / 256.0f, 1e-6f);
    EXPECT_NEAR(fb.jitter_ms, 2.0f, 1e-6f);
    EXPECT_EQ(fb.rtt_ms, -1.0f);

    // Second: 50 more expected, 5 more lost, with a usable LSR.
    r.extended_highest_seq += 50;
    r.cumulative_lost = 7;
    r.fraction_lost = 25;
    r.last_sr = now - 0x4000u;
    r.delay_since_last_sr = 0x3000u;
    fb = tracker.on_report(r, now, 48000);
    EXPECT_EQ(fb.reports, 3u);
    EXPECT_EQ(fb.interval_expected, 50u);
    EXPECT_EQ(fb.interval_lost, 5u);
    EXPECT_NEAR(fb.rtt_ms, 1000.0f * 0x1000 / 65536.0f, 0.1f);

    // Third: the cumulative count went down (duplicates). The loss falls
    // back to the fraction: 64/256 of 40.
    r.extended_highest_seq += 40;
    r.cumulative_lost = 1;
    r.fraction_lost = 64;
    fb = tracker.on_report(r, now, 48000);
    EXPECT_EQ(fb.interval_expected, 40u);
    EXPECT_EQ(fb.interval_lost, 10u);

    // Fourth: no new packets (the speech gate is closed).
    fb = tracker.on_report(r, now, 48000);
    EXPECT_EQ(fb.interval_expected, 0u);
    EXPECT_EQ(fb.interval_lost, 0u);

    // Fifth: the sequence went back (a restarted reporter): no interval.
    r.extended_highest_seq -= 100;
    fb = tracker.on_report(r, now, 48000);
    EXPECT_EQ(fb.interval_expected, 0u);
    EXPECT_EQ(fb.interval_lost, 0u);

    MelloSendFeedback latest{};
    ASSERT_TRUE(tracker.latest(&latest));
    EXPECT_EQ(latest.reports, 6u);
    EXPECT_EQ(latest.cumulative_lost, 1);
}

// --- Wire test -------------------------------------------------------------

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

// Answers each sender report with a receiver report about the sender, as
// Pion's receiver interceptor reports: fraction lost 1/4, the cumulative
// count up by 3 each time, LSR from the sender report, DLSR 0.
class CraftedReceiverReports final : public rtc::MediaHandler {
public:
    void incoming(rtc::message_vector& messages, const rtc::message_callback& send) override {
        for (const auto& message : messages) {
            if (!message || message->type != rtc::Message::Control || message->size() < 28) {
                continue;
            }
            const auto* b = reinterpret_cast<const uint8_t*>(message->data());
            if (b[1] != 200) continue;
            ReceptionReport block;
            block.source_ssrc = read_be32(b + 4);
            block.fraction_lost = 64;
            block.cumulative_lost = 3 * ++answered_;
            block.extended_highest_seq = 1000u + 50u * static_cast<uint32_t>(answered_);
            block.jitter = 240;
            // Middle 32 bits of the 64-bit NTP time at bytes 8..15.
            block.last_sr = (read_be32(b + 8) << 16) | (read_be32(b + 12) >> 16);
            block.delay_since_last_sr = 0;
            const auto rr = receiver_report(0xC0FFEE01u, {block});
            auto reply = rtc::make_message(rr.size(), rtc::Message::Control);
            std::memcpy(reply->data(), rr.data(), rr.size());
            send(reply);
        }
    }

private:
    int answered_ = 0;
};

struct FeedbackLog {
    std::mutex mutex;
    std::vector<MelloSendFeedback> items;
};

void on_feedback(void* user_data, const MelloSendFeedback* feedback) {
    if (!user_data || !feedback) return;
    auto* log = static_cast<FeedbackLog*>(user_data);
    std::lock_guard<std::mutex> lock(log->mutex);
    log->items.push_back(*feedback);
}

TEST(VoiceSendFeedbackWire, ReceiverReportsReachTheVoicePeer) {
    CandidateQueue voice_candidates;
    FeedbackLog log;

    MelloPeerConnection* voice = mello_peer_create(nullptr, "voice-peer");
    ASSERT_NE(voice, nullptr);
    mello_peer_set_ice_servers(voice, nullptr, 0);
    mello_peer_set_ice_callback(voice, on_voice_candidate, &voice_candidates);
    mello_peer_set_send_feedback_callback(voice, on_feedback, &log);
    MelloSendFeedback none{};
    EXPECT_FALSE(mello_peer_get_send_feedback(voice, &none));
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
        track->setMediaHandler(std::make_shared<CraftedReceiverReports>());
        track->onMessage([](rtc::binary) {}, [](rtc::string) {});
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

    const uint8_t probe[2] = {0xFF, 0};
    ASSERT_TRUE(wait_until(
        [&]() { return mello_peer_send_audio_frame(voice, probe, 2, 0) == MELLO_OK; }, 5s));

    // rtc::RtcpSrReporter sends a sender report at most once per second,
    // with the outgoing frames. 2.2 s of frames give at least two.
    for (uint32_t i = 1; i <= 110; ++i) {
        const uint8_t frame[2] = {static_cast<uint8_t>(i), 0};
        mello_peer_send_audio_frame(voice, frame, 2, i * 960);
        std::this_thread::sleep_for(20ms);
    }
    ASSERT_TRUE(wait_until(
        [&]() {
            std::lock_guard<std::mutex> lock(log.mutex);
            return log.items.size() >= 2;
        },
        5s));

    {
        std::lock_guard<std::mutex> lock(log.mutex);
        const auto& first = log.items[0];
        const auto& second = log.items[1];
        EXPECT_EQ(first.reports, 1u);
        EXPECT_EQ(second.reports, 2u);
        EXPECT_NEAR(second.fraction_lost, 0.25f, 1e-6f);
        EXPECT_EQ(second.cumulative_lost, 6);
        EXPECT_NEAR(second.jitter_ms, 5.0f, 1e-6f);
        EXPECT_EQ(second.interval_expected, 50u);
        EXPECT_EQ(second.interval_lost, 3u);
        // Localhost: the round trip is real and short.
        EXPECT_GE(second.rtt_ms, 0.0f);
        EXPECT_LT(second.rtt_ms, 500.0f);
    }
    MelloSendFeedback latest{};
    ASSERT_TRUE(mello_peer_get_send_feedback(voice, &latest));
    EXPECT_GE(latest.reports, 2u);

    mello_peer_set_send_feedback_callback(voice, nullptr, nullptr);
    mello_peer_destroy(voice);
    remote->close();
    {
        std::lock_guard<std::mutex> lock(tracks_mutex);
        tracks.clear();
    }
    remote.reset();
}

}  // namespace
