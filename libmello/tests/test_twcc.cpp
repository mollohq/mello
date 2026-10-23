#include <gtest/gtest.h>

#include "transport/twcc.hpp"

#include <cstdint>
#include <vector>

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
    uint16_t sequence = 99;
    ASSERT_TRUE(stamper.stamp(packet, sequence));
    EXPECT_EQ(sequence, 0u);

    ASSERT_EQ(packet.size(), 100u + 12u + 8u);
    EXPECT_EQ(packet[0] & 0x10, 0x10); // X bit set
    EXPECT_EQ(packet[12], 0xBE);
    EXPECT_EQ(packet[13], 0xDE);
    EXPECT_EQ(packet[15], 1u); // one word of body
    EXPECT_EQ(packet[16] >> 4, mello::transport::kTwccExtensionId);
    EXPECT_EQ(packet[17], 0u);
    EXPECT_EQ(packet[18], 0u); // first sequence = 0

    auto second = make_rtp_packet(101, 50);
    ASSERT_TRUE(stamper.stamp(second, sequence));
    EXPECT_EQ(sequence, 1u);
    EXPECT_EQ(
        static_cast<unsigned>(second[17] << 8 | second[18]),
        1u
    ); // second sequence = 1
    EXPECT_EQ(stamper.next_sequence(), 2u);
}

TEST(TwccStamperTest, RestampOverwritesSequenceInPlace) {
    TwccSendStamper stamper;
    auto packet = make_rtp_packet(200, 80);
    uint16_t sequence = 0;
    ASSERT_TRUE(stamper.stamp(packet, sequence));
    const size_t size_after_first = packet.size();

    ASSERT_TRUE(stamper.stamp(packet, sequence));
    EXPECT_EQ(sequence, 1u);
    EXPECT_EQ(packet.size(), size_after_first);
    EXPECT_EQ(
        static_cast<unsigned>(packet[17] << 8 | packet[18]),
        1u
    ); // re-stamped with seq 1
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
        EXPECT_NEAR(static_cast<double>(spacing), 1000.0, 500.0);
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

// The bytes of one report, checked against the transport-cc draft
// (draft-holmer-rmcat-transport-wide-cc-extensions-01, section 3.1). The
// same bytes are the input of the mello-sfu test that parses them with Pion
// (internal/server/twcc_feedback_parse_test.go): keep the two equal.
//
// Input: sequences 100 to 115 arrive 1 ms apart from 64.003 s, 116 is lost,
// 117 arrives 70 ms after 115.
const std::vector<uint8_t> kLayoutReport = {
    0x8f, 0xcd, 0x00, 0x0a, // V=2, FMT=15, PT=205, length 10 words (44 bytes)
    0x11, 0x11, 0x11, 0x11, // sender SSRC
    0x22, 0x22, 0x22, 0x22, // media SSRC
    0x00, 0x64, 0x00, 0x12, // base sequence 100, status count 18
    0x00, 0x03, 0xe8, 0x00, // reference time 1000 x 64 ms, feedback count 0
    0x20, 0x10,             // run length chunk: S=1 (small delta), run 16
    0xc8, 0x00,             // two-bit vector: 0 (lost), 2 (large delta), 5 x 0
    0x0c,                   // 100: 3 ms after the reference (12 x 250 us)
    0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, // 101 to 115: 1 ms each
    0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04,
    0x01, 0x18,             // 117: 70 ms, large delta 280
    0x00, 0x00,             // padding to 32 bits
};

std::vector<uint8_t> build_layout_report() {
    TwccFeedbackGenerator generator;
    const int64_t t0 = 64'003'000;
    for (uint16_t seq = 100; seq <= 115; ++seq) {
        generator.on_packet(seq, t0 + (seq - 100) * 1'000);
    }
    generator.on_packet(117, t0 + 15'000 + 70'000);
    return generator.build_feedback(0x11111111, 0x22222222);
}

TEST(TwccFeedbackLayoutTest, ReportBytesMatchTheTransportCcLayout) {
    EXPECT_EQ(build_layout_report(), kLayoutReport);
}

TEST(TwccFeedbackLayoutTest, LayoutReportParsesBackToTheArrivalTimes) {
    TwccFeedback feedback;
    ASSERT_TRUE(parse_twcc_feedback(
        kLayoutReport.data(), kLayoutReport.size(), feedback));
    EXPECT_EQ(feedback.reference_time_64ms, 1000u);
    ASSERT_EQ(feedback.packets.size(), 18u);
    for (size_t i = 0; i < 16; ++i) {
        EXPECT_TRUE(feedback.packets[i].received);
        EXPECT_EQ(feedback.packets[i].arrival_time_us,
                  64'003'000 + static_cast<int64_t>(i) * 1'000);
    }
    EXPECT_FALSE(feedback.packets[16].received);
    EXPECT_TRUE(feedback.packets[17].received);
    EXPECT_EQ(feedback.packets[17].arrival_time_us, 64'003'000 + 85'000);
}

// The old generator took the reference time rounded down to 64 ms but the
// first delta against the exact arrival, so every report carried an offset
// of 0 to 64 ms. The reconstructed time must be the arrival time.
TEST(TwccFeedbackLayoutTest, TheFirstDeltaIsOnTheReferenceClock) {
    for (const int64_t t0 : {64'000'000LL, 64'030'000LL, 64'063'750LL}) {
        TwccFeedbackGenerator generator;
        generator.on_packet(7, t0);
        generator.on_packet(8, t0 + 2'000);
        const auto bytes = generator.build_feedback(1, 2);
        TwccFeedback feedback;
        ASSERT_TRUE(parse_twcc_feedback(bytes.data(), bytes.size(), feedback));
        ASSERT_EQ(feedback.packets.size(), 2u);
        EXPECT_EQ(feedback.packets[0].arrival_time_us, t0);
        EXPECT_EQ(feedback.packets[1].arrival_time_us, t0 + 2'000);
    }
}

// Deltas over 31.75 ms (and negative ones from a reorder) are reported, not
// clamped: the old generator clamped them to a signed byte.
TEST(TwccFeedbackLayoutTest, LargeAndNegativeDeltasAreNotClamped) {
    TwccFeedbackGenerator generator;
    const int64_t t0 = 128'000'000;
    generator.on_packet(1, t0);
    generator.on_packet(2, t0 + 100'000);  // 100 ms later
    generator.on_packet(3, t0 + 90'000);   // arrived before 2
    generator.on_packet(4, t0 + 2'000'000); // 2 s later
    const auto bytes = generator.build_feedback(1, 2);
    TwccFeedback feedback;
    ASSERT_TRUE(parse_twcc_feedback(bytes.data(), bytes.size(), feedback));
    ASSERT_EQ(feedback.packets.size(), 4u);
    EXPECT_EQ(feedback.packets[0].arrival_time_us, t0);
    EXPECT_EQ(feedback.packets[1].arrival_time_us, t0 + 100'000);
    EXPECT_EQ(feedback.packets[2].arrival_time_us, t0 + 90'000);
    EXPECT_EQ(feedback.packets[3].arrival_time_us, t0 + 2'000'000);
}

// The old generator reported only the newest 64 packets of a report period.
TEST(TwccFeedbackLayoutTest, OneReportCarriesMoreThan64Packets) {
    TwccFeedbackGenerator generator;
    const int64_t t0 = 192'000'000;
    for (uint16_t seq = 0; seq < 200; ++seq) {
        if (seq % 9 != 4) {
            generator.on_packet(seq, t0 + seq * 250);
        }
    }
    const auto bytes = generator.build_feedback(1, 2);
    EXPECT_EQ(generator.pending(), 0u);
    TwccFeedback feedback;
    ASSERT_TRUE(parse_twcc_feedback(bytes.data(), bytes.size(), feedback));
    ASSERT_EQ(feedback.packets.size(), 200u);
    for (uint16_t seq = 0; seq < 200; ++seq) {
        EXPECT_EQ(feedback.packets[seq].sequence, seq);
        EXPECT_EQ(feedback.packets[seq].received, seq % 9 != 4);
        if (feedback.packets[seq].received) {
            EXPECT_EQ(feedback.packets[seq].arrival_time_us, t0 + seq * 250);
        }
    }
}

// A period with more packets than one report holds leaves the rest pending;
// the next reports continue at the next sequence, across the 16-bit wrap.
TEST(TwccFeedbackLayoutTest, ALongPeriodSplitsIntoReportsThatCoverEverySequence) {
    TwccFeedbackGenerator generator;
    const int64_t t0 = 256'000'000;
    const uint16_t first = 65'000;
    for (int i = 0; i < 3'000; ++i) {
        generator.on_packet(static_cast<uint16_t>(first + i), t0 + i * 400);
    }
    std::vector<uint16_t> covered;
    int reports = 0;
    while (generator.pending() > 0) {
        const auto bytes = generator.build_feedback(1, 2);
        ASSERT_FALSE(bytes.empty());
        EXPECT_LE(bytes.size(), 1'100u);
        ++reports;
        TwccFeedback feedback;
        ASSERT_TRUE(parse_twcc_feedback(bytes.data(), bytes.size(), feedback));
        for (const auto& packet : feedback.packets) {
            ASSERT_TRUE(packet.received);
            covered.push_back(packet.sequence);
        }
        ASSERT_LT(reports, 100);
    }
    EXPECT_GT(reports, 1);
    ASSERT_EQ(covered.size(), 3'000u);
    for (int i = 0; i < 3'000; ++i) {
        EXPECT_EQ(covered[static_cast<size_t>(i)], static_cast<uint16_t>(first + i));
    }
}

} // namespace
