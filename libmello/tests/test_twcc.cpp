#include <gtest/gtest.h>

#include "transport/twcc.hpp"

#include <cstdint>
#include <vector>

using mello::transport::GccEstimator;
using mello::transport::parse_twcc_feedback;
using mello::transport::TwccFeedback;
using mello::transport::TwccFeedbackGenerator;
using mello::transport::TwccSendStamper;

namespace {

std::vector<uint8_t> make_rtp_packet(uint16_t sequence, size_t payload_bytes) {
    std::vector<uint8_t> packet(12 + payload_bytes, 0);
    packet[0] = 0x80; // V2, no ext, no csrc
    packet[1] = 96;
    packet[2] = static_cast<uint8_t>(sequence >> 8);
    packet[3] = static_cast<uint8_t>(sequence & 0xff);
    for (size_t i = 12; i < packet.size(); ++i) {
        packet[i] = static_cast<uint8_t>(i & 0xff);
    }
    return packet;
}

TEST(TwccStamperTest, InsertsExtensionBlockIntoPlainRtpPacket) {
    TwccSendStamper stamper;
    auto packet = make_rtp_packet(100, 100);
    ASSERT_TRUE(stamper.stamp(packet, 1'000'000));

    ASSERT_EQ(packet.size(), 100u + 12u + 8u);
    EXPECT_EQ(packet[0] & 0x10, 0x10); // X bit set
    EXPECT_EQ(packet[12], 0xBE);
    EXPECT_EQ(packet[13], 0xDE);
    EXPECT_EQ(packet[15], 1u); // one word of body
    EXPECT_EQ(packet[16] >> 4, mello::transport::kTwccExtensionId);
    EXPECT_EQ(packet[17], 0u);
    EXPECT_EQ(packet[18], 0u); // first sequence = 0

    auto second = make_rtp_packet(101, 50);
    ASSERT_TRUE(stamper.stamp(second, 1'001'000));
    EXPECT_EQ(second[17] << 8 | second[18], 1u); // second sequence = 1

    int64_t send_time = -1;
    ASSERT_TRUE(stamper.send_time_for(0, send_time));
    EXPECT_EQ(send_time, 1'000'000);
    ASSERT_TRUE(stamper.send_time_for(1, send_time));
    EXPECT_EQ(send_time, 1'001'000);
    EXPECT_FALSE(stamper.send_time_for(42, send_time));
}

TEST(TwccStamperTest, RestampOverwritesSequenceInPlace) {
    TwccSendStamper stamper;
    auto packet = make_rtp_packet(200, 80);
    ASSERT_TRUE(stamper.stamp(packet, 1'000'000));
    const size_t size_after_first = packet.size();

    ASSERT_TRUE(stamper.stamp(packet, 2'000'000));
    EXPECT_EQ(packet.size(), size_after_first);
    EXPECT_EQ(packet[17] << 8 | packet[18], 1u); // re-stamped with seq 1
}

TEST(TwccFeedbackRoundTripTest, GeneratorOutputParsesBack) {
    TwccFeedbackGenerator generator;
    const int64_t t0 = 10'000'000;
    for (uint16_t seq = 0; seq < 10; ++seq) {
        generator.on_packet(seq, t0 + seq * 1'000); // 1ms apart
    }
    const auto bytes = generator.build_feedback(0x11111111, 0x22222222);
    ASSERT_FALSE(bytes.empty());
    EXPECT_EQ(generator.pending(), 0u);

    TwccFeedback feedback;
    ASSERT_TRUE(parse_twcc_feedback(bytes.data(), bytes.size(), feedback));
    EXPECT_EQ(feedback.sender_ssrc, 0x11111111u);
    EXPECT_EQ(feedback.media_ssrc, 0x22222222u);
    ASSERT_EQ(feedback.packets.size(), 10u);
    for (size_t i = 0; i < 10; ++i) {
        EXPECT_EQ(feedback.packets[i].sequence, i);
        EXPECT_TRUE(feedback.packets[i].received);
    }
    // Reference-time quantization (64 ms) shifts all arrivals by one shared
    // constant; pairwise spacing must survive within one delta unit (250us).
    for (size_t i = 1; i < 10; ++i) {
        const int64_t spacing =
            feedback.packets[i].arrival_time_us
            - feedback.packets[i - 1].arrival_time_us;
        EXPECT_NEAR(spacing, 1'000, 500);
    }
}

TEST(TwccFeedbackRoundTripTest, MissingPacketsReportAsNotReceived) {
    TwccFeedbackGenerator generator;
    const int64_t t0 = 20'000'000;
    generator.on_packet(5, t0);
    generator.on_packet(7, t0 + 2'000);
    generator.on_packet(8, t0 + 3'000);

    const auto bytes = generator.build_feedback(1, 2);
    TwccFeedback feedback;
    ASSERT_TRUE(parse_twcc_feedback(bytes.data(), bytes.size(), feedback));
    ASSERT_EQ(feedback.packets.size(), 4u); // 5,6,7,8
    EXPECT_TRUE(feedback.packets[0].received);
    EXPECT_FALSE(feedback.packets[1].received); // seq 6 lost
    EXPECT_TRUE(feedback.packets[2].received);
    EXPECT_TRUE(feedback.packets[3].received);
}

TEST(GccEstimatorTest, StableDelayRampsUp) {
    GccEstimator::Config config;
    config.min_bps = 100'000;
    config.max_bps = 10'000'000;
    GccEstimator estimator(config, 4'000'000);

    // One group per packet (send gap 10ms > 5ms window), constant delay.
    for (int i = 1; i < 40; ++i) {
        const int64_t send = static_cast<int64_t>(i) * 10'000;
        estimator.on_packet(
            static_cast<uint16_t>(i),
            true,
            send,
            send + 8'000);
    }
    EXPECT_GT(estimator.target_bps(), 4'000'000u);
}

TEST(GccEstimatorTest, DelayBuildupTriggersDecrease) {
    GccEstimator::Config config;
    config.min_bps = 100'000;
    config.max_bps = 10'000'000;
    GccEstimator estimator(config, 4'000'000);

    // Each group arrives progressively later: queueing delay growth of
    // ~2ms per group, sustained — must be detected as overuse.
    const uint64_t before = estimator.target_bps();
    for (int i = 1; i < 200; ++i) {
        const int64_t send = static_cast<int64_t>(i) * 10'000;
        const int64_t arrival = send + 8'000 + static_cast<int64_t>(i) * 2'000;
        estimator.on_packet(static_cast<uint16_t>(i), true, send, arrival);
    }
    EXPECT_LT(estimator.target_bps(), before);
}

TEST(GccEstimatorTest, SustainedLossCapsTarget) {
    GccEstimator::Config config;
    config.min_bps = 100'000;
    config.max_bps = 10'000'000;
    GccEstimator estimator(config, 4'000'000);

    // 25% loss: every fourth packet lost, no delay growth.
    const uint64_t before = estimator.target_bps();
    for (int i = 1; i < 400; ++i) {
        const bool received = (i % 4) != 0;
        const int64_t send = static_cast<int64_t>(i) * 10'000;
        estimator.on_packet(
            static_cast<uint16_t>(i),
            received,
            send,
            send + 8'000);
    }
    EXPECT_GT(estimator.loss_rate(), 0.10);
    EXPECT_LT(estimator.target_bps(), before);
}

} // namespace
