#include <gtest/gtest.h>

#include "transport/rtp_h264_receiver.hpp"

#include <chrono>
#include <cstdint>
#include <initializer_list>
#include <optional>
#include <utility>
#include <vector>

using mello::transport::RtpH264Receiver;

namespace {

using namespace std::chrono_literals;

struct RtpOptions {
    uint8_t payload_type = 96;
    uint8_t csrc_count = 0;
    std::vector<uint8_t> extension;
    uint8_t padding = 0;
    uint32_t ssrc = 0;
};

RtpH264Receiver::TimePoint at(int milliseconds) {
    return RtpH264Receiver::TimePoint{} +
           std::chrono::milliseconds(milliseconds);
}

std::vector<uint8_t> make_rtp(uint16_t sequence,
                              uint32_t timestamp,
                              bool marker,
                              const std::vector<uint8_t>& payload,
                              const RtpOptions& options = {}) {
    uint8_t first = static_cast<uint8_t>(0x80 | options.csrc_count);
    if (!options.extension.empty()) {
        first |= 0x10;
    }
    if (options.padding != 0) {
        first |= 0x20;
    }

    std::vector<uint8_t> packet(12, 0);
    packet[0] = first;
    packet[1] = static_cast<uint8_t>(
        (marker ? 0x80 : 0) | (options.payload_type & 0x7f));
    packet[2] = static_cast<uint8_t>(sequence >> 8);
    packet[3] = static_cast<uint8_t>(sequence);
    packet[4] = static_cast<uint8_t>(timestamp >> 24);
    packet[5] = static_cast<uint8_t>(timestamp >> 16);
    packet[6] = static_cast<uint8_t>(timestamp >> 8);
    packet[7] = static_cast<uint8_t>(timestamp);
    packet[8] = static_cast<uint8_t>(options.ssrc >> 24);
    packet[9] = static_cast<uint8_t>(options.ssrc >> 16);
    packet[10] = static_cast<uint8_t>(options.ssrc >> 8);
    packet[11] = static_cast<uint8_t>(options.ssrc);

    for (uint8_t i = 0; i < options.csrc_count; ++i) {
        packet.push_back(0);
        packet.push_back(0);
        packet.push_back(0);
        packet.push_back(i);
    }

    if (!options.extension.empty()) {
        const uint16_t words =
            static_cast<uint16_t>(options.extension.size() / 4);
        packet.push_back(0xbe);
        packet.push_back(0xde);
        packet.push_back(static_cast<uint8_t>(words >> 8));
        packet.push_back(static_cast<uint8_t>(words));
        packet.insert(packet.end(),
                      options.extension.begin(),
                      options.extension.end());
    }

    packet.insert(packet.end(), payload.begin(), payload.end());
    if (options.padding != 0) {
        packet.insert(packet.end(), options.padding - 1, 0);
        packet.push_back(options.padding);
    }
    return packet;
}

// A pacer's padding packet: a header and padding, no payload.
std::vector<uint8_t> make_padding(uint16_t sequence, uint32_t timestamp) {
    RtpOptions options;
    options.padding = 8;
    return make_rtp(sequence, timestamp, false, {}, options);
}

std::vector<uint8_t> make_stap(
    std::initializer_list<std::vector<uint8_t>> nals) {
    std::vector<uint8_t> payload{0x78};
    for (const auto& nal : nals) {
        payload.push_back(static_cast<uint8_t>(nal.size() >> 8));
        payload.push_back(static_cast<uint8_t>(nal.size()));
        payload.insert(payload.end(), nal.begin(), nal.end());
    }
    return payload;
}

std::vector<uint8_t> annex_b(
    std::initializer_list<std::vector<uint8_t>> nals) {
    std::vector<uint8_t> bytes;
    for (const auto& nal : nals) {
        bytes.insert(bytes.end(), {0, 0, 0, 1});
        bytes.insert(bytes.end(), nal.begin(), nal.end());
    }
    return bytes;
}

// An IDR access unit in one packet (SPS, PPS, IDR slice at macroblock 0).
std::vector<uint8_t> idr_payload() {
    return make_stap({{0x67, 0x42}, {0x68, 0xce}, {0x65, 0x88}});
}

void deliver(RtpH264Receiver& receiver,
             std::vector<uint8_t> packet,
             RtpH264Receiver::TimePoint now) {
    receiver.on_rtp_packet(packet.data(), packet.size(), now);
}

// A receiver with every callback recorded. `render_ms` answers the repair
// deadline's render time query: the RTP timestamp in ms (90 kHz) plus the
// offset, or nothing.
class RtpH264ReceiverTest : public ::testing::Test {
protected:
    RtpH264ReceiverTest() : receiver(make_callbacks(), config()) {}

    static RtpH264Receiver::Config config() {
        RtpH264Receiver::Config c;
        c.rtt = 20ms;
        return c;
    }

    RtpH264Receiver::Callbacks make_callbacks() {
        RtpH264Receiver::Callbacks callbacks;
        callbacks.on_access_unit = [this](RtpH264Receiver::AccessUnit&& unit) {
            units.push_back(std::move(unit));
        };
        callbacks.on_nack = [this](const std::vector<uint16_t>& sequences) {
            nacks.push_back(sequences);
        };
        callbacks.on_loss = [this](int64_t sequence) { losses.push_back(sequence); };
        callbacks.render_time =
            [this](uint32_t rtp) -> std::optional<RtpH264Receiver::TimePoint> {
            if (!render_offset_ms) return std::nullopt;
            return at(static_cast<int>(rtp / 90) + *render_offset_ms);
        };
        return callbacks;
    }

    // Seq 10, timestamp 0: an IDR that opens the stream.
    void unlock() {
        deliver(receiver, make_rtp(10, 0, true, idr_payload()), at(0));
        ASSERT_EQ(units.size(), 1u);
        ASSERT_TRUE(units[0].keyframe);
    }

    std::vector<RtpH264Receiver::AccessUnit> units;
    std::vector<std::vector<uint16_t>> nacks;
    std::vector<int64_t> losses;
    std::optional<int> render_offset_ms = 30;
    RtpH264Receiver receiver;
};

TEST_F(RtpH264ReceiverTest, ReconstructsZeroLossAccessUnit) {
    RtpOptions options;
    options.csrc_count = 2;
    options.extension = {0x10, 0x20, 0x30, 0x40};
    options.padding = 4;

    deliver(receiver, make_rtp(100, 9000, false, {0x67, 0x42}, options), at(0));
    deliver(receiver, make_rtp(101, 9000, false, {0x68, 0xce}), at(0));
    deliver(receiver, make_rtp(102, 9000, true, {0x65, 0xaa, 0xbb}), at(0));

    ASSERT_EQ(units.size(), 1u);
    EXPECT_EQ(units[0].annex_b,
              annex_b({{0x67, 0x42}, {0x68, 0xce}, {0x65, 0xaa, 0xbb}}));
    EXPECT_TRUE(units[0].keyframe);
    EXPECT_EQ(units[0].rtp_timestamp, 9000u);
    EXPECT_EQ(units[0].first_sequence, 100);
    EXPECT_EQ(units[0].last_sequence, 102);
    EXPECT_EQ(units[0].reference, -1);

    const auto stats = receiver.stats();
    EXPECT_EQ(stats.packets, 3u);
    EXPECT_EQ(stats.accepted_packets, 3u);
    EXPECT_EQ(stats.complete_access_units, 1u);
    EXPECT_EQ(stats.emitted_access_units, 1u);
}

TEST(RtpH264ReceiverConfigurationTest, AcceptsConfiguredPayloadType) {
    std::vector<RtpH264Receiver::AccessUnit> units;
    RtpH264Receiver::Callbacks callbacks;
    callbacks.on_access_unit = [&units](RtpH264Receiver::AccessUnit&& unit) {
        units.push_back(std::move(unit));
    };
    RtpH264Receiver::Config config;
    config.payload_type = 110;
    RtpH264Receiver configured(std::move(callbacks), config);

    RtpOptions options;
    options.payload_type = 110;
    deliver(configured, make_rtp(1, 1234, true, idr_payload(), options), at(0));

    ASSERT_EQ(units.size(), 1u);
    EXPECT_EQ(units[0].rtp_timestamp, 1234u);
    EXPECT_EQ(configured.stats().wrong_payload_type_packets, 0u);
}

// The sender's loopback tests hand the older callback form; it still works.
TEST(RtpH264ReceiverConfigurationTest, TheOlderCallbackFormGetsTheAccessUnit) {
    std::vector<std::vector<uint8_t>> bytes;
    RtpH264Receiver receiver(RtpH264Receiver::Callbacks{
        [&bytes](const std::vector<uint8_t>& annex_b, bool, uint32_t) { bytes.push_back(annex_b); },
        {},
        {},
    });
    deliver(receiver, make_rtp(1, 0, true, idr_payload()), at(0));
    ASSERT_EQ(bytes.size(), 1u);
}

TEST(RtpH264ReceiverSourceTest, LearnsAndStrictlyFiltersSsrc) {
    RtpH264Receiver receiver;
    RtpOptions source;
    source.ssrc = 0x11223344;
    RtpOptions other_source;
    other_source.ssrc = 0x55667788;

    const auto first = make_rtp(1, 1000, false, {0x61, 0x81});
    auto first_with_ssrc = make_rtp(1, 1000, false, {0x61, 0x81}, source);
    const auto foreign = make_rtp(2, 1000, true, {0x61, 0x02}, other_source);
    const auto second = make_rtp(2, 1000, true, {0x61, 0x02}, source);
    (void)first;
    const size_t received_bytes = first_with_ssrc.size() + foreign.size() + second.size();
    const size_t accepted_bytes = first_with_ssrc.size() + second.size();

    deliver(receiver, first_with_ssrc, at(0));
    deliver(receiver, foreign, at(1));
    deliver(receiver, second, at(2));

    const auto stats = receiver.stats();
    EXPECT_TRUE(stats.has_ssrc);
    EXPECT_EQ(stats.ssrc, source.ssrc);
    EXPECT_EQ(stats.packets, 3u);
    EXPECT_EQ(stats.accepted_packets, 2u);
    EXPECT_EQ(stats.wrong_ssrc_packets, 1u);
    EXPECT_EQ(stats.bytes_received, received_bytes);
    EXPECT_EQ(stats.accepted_bytes, accepted_bytes);
    EXPECT_EQ(stats.extended_highest_sequence, 2u);
    EXPECT_EQ(stats.cumulative_loss, 0);
}

TEST(RtpH264ReceiverSourceTest, InvalidPayloadDoesNotSelectSsrc) {
    RtpH264Receiver receiver;
    RtpOptions invalid_source;
    invalid_source.ssrc = 0x11111111;
    RtpOptions valid_source;
    valid_source.ssrc = 0x22222222;

    deliver(receiver, make_rtp(1, 1000, true, {0x79, 0x01}, invalid_source), at(0));
    deliver(receiver, make_rtp(1, 1000, true, {0x61, 0x81}, valid_source), at(1));

    const auto stats = receiver.stats();
    EXPECT_TRUE(stats.has_ssrc);
    EXPECT_EQ(stats.ssrc, valid_source.ssrc);
    EXPECT_EQ(stats.invalid_h264_packets, 1u);
    EXPECT_EQ(stats.wrong_ssrc_packets, 0u);
    EXPECT_EQ(stats.accepted_packets, 1u);
}

TEST_F(RtpH264ReceiverTest, RejectsBackwardsInputsWithoutMovingDeadlines) {
    unlock();
    deliver(receiver, make_rtp(11, 1000, false, {0x61, 0x81}), at(10));
    deliver(receiver, make_rtp(13, 1000, true, {0x61, 0x03}), at(10));
    deliver(receiver, make_rtp(12, 1000, false, {0x61, 0x02}), at(9));
    receiver.tick(at(8));

    EXPECT_TRUE(nacks.empty());
    EXPECT_EQ(receiver.stats().backwards_time_inputs, 2u);
    EXPECT_EQ(receiver.stats().repaired_packets, 0u);

    receiver.tick(at(13));
    ASSERT_EQ(nacks.size(), 1u);
    EXPECT_EQ(nacks[0], std::vector<uint16_t>({12}));
}

TEST(RtpH264ReceiverStatsTest, ComputesRfc3550InterarrivalJitter) {
    RtpH264Receiver receiver;

    deliver(receiver, make_rtp(1, 0, true, {0x61, 0x81}), at(0));
    deliver(receiver, make_rtp(2, 900, true, {0x61, 0x82}), at(10));
    EXPECT_EQ(receiver.stats().interarrival_jitter, 0u);

    deliver(receiver, make_rtp(3, 1800, true, {0x61, 0x83}), at(30));
    EXPECT_EQ(receiver.stats().interarrival_jitter, 56u);

    deliver(receiver, make_rtp(4, 2700, true, {0x61, 0x84}), at(40));
    EXPECT_EQ(receiver.stats().interarrival_jitter, 52u);
}

TEST_F(RtpH264ReceiverTest, ReassemblesFuAAndReferencesThePreviousAccessUnit) {
    unlock();

    deliver(receiver, make_rtp(11, 1500, false, {0x7c, 0x81, 0xaa}), at(4));
    deliver(receiver, make_rtp(12, 1500, false, {0x7c, 0x01, 0xbb}), at(4));
    deliver(receiver, make_rtp(13, 1500, true, {0x7c, 0x41, 0xcc}), at(4));

    ASSERT_EQ(units.size(), 2u);
    EXPECT_EQ(units[1].annex_b, annex_b({{0x61, 0xaa, 0xbb, 0xcc}}));
    EXPECT_FALSE(units[1].keyframe);
    EXPECT_EQ(units[1].first_sequence, 11);
    EXPECT_EQ(units[1].last_sequence, 13);
    EXPECT_EQ(units[1].reference, 10) << "a delta frame decodes from the access unit before it";
    EXPECT_TRUE(nacks.empty());
}

TEST_F(RtpH264ReceiverTest, UnpacksStapAAtOnce) {
    deliver(receiver, make_rtp(20, 3000, true,
                               make_stap({{0x67, 0x01}, {0x68, 0x02}, {0x65, 0x83}})),
            at(0));
    ASSERT_EQ(units.size(), 1u) << "a complete access unit leaves at once, no reorder wait";
    EXPECT_EQ(units[0].annex_b, annex_b({{0x67, 0x01}, {0x68, 0x02}, {0x65, 0x83}}));
    EXPECT_TRUE(units[0].keyframe);
}

TEST_F(RtpH264ReceiverTest, ReordersWithinAnAccessUnit) {
    unlock();

    deliver(receiver, make_rtp(12, 1500, true, {0x61, 0x12}), at(4));
    EXPECT_EQ(units.size(), 1u);
    deliver(receiver, make_rtp(11, 1500, false, {0x61, 0x91}), at(5));

    ASSERT_EQ(units.size(), 2u);
    EXPECT_EQ(units[1].annex_b, annex_b({{0x61, 0x91}, {0x61, 0x12}}));
    EXPECT_TRUE(nacks.empty());
}

// Stage 3: a complete access unit is not held behind an older incomplete one
// (the old receiver emitted strictly in order, and its 3-AU window evicted
// the incomplete one, gated the stream and asked for a key frame).
TEST_F(RtpH264ReceiverTest, ACompleteAccessUnitLeavesBeforeAnOlderIncompleteOne) {
    unlock();
    // AU 1500: seq 11..13, seq 12 lost for now.
    deliver(receiver, make_rtp(11, 1500, false, {0x7c, 0x81, 0xaa}), at(16));
    deliver(receiver, make_rtp(13, 1500, true, {0x7c, 0x41, 0xcc}), at(16));
    // Four newer access units arrive complete.
    for (int i = 0; i < 4; ++i) {
        deliver(receiver, make_rtp(static_cast<uint16_t>(14 + i), 3000 + 1500 * i, true, {0x61, 0x81}),
                at(17 + i));
    }
    ASSERT_EQ(units.size(), 5u) << "four newer AUs are out while seq 12 is repaired";
    EXPECT_EQ(units[1].first_sequence, 14);
    EXPECT_EQ(units[1].reference, 13) << "they reference the incomplete AU: the timeline orders them";
    EXPECT_GE(receiver.stats().out_of_order_access_units, 4u);
    EXPECT_TRUE(losses.empty());

    // The repair completes the older one; nothing was given up.
    deliver(receiver, make_rtp(12, 1500, false, {0x7c, 0x01, 0xbb}), at(40));
    ASSERT_EQ(units.size(), 6u);
    EXPECT_EQ(units[5].first_sequence, 11);
    EXPECT_TRUE(units[5].retransmitted);
    EXPECT_EQ(receiver.stats().incomplete_access_units, 0u);
}

// Stage 3: NACK repeats every max(RTT, 10 ms) until the repair deadline
// (render time of the frame after the gap plus one RTT, and not before the
// repair window has passed since the gap was seen), not twice.
TEST_F(RtpH264ReceiverTest, NacksEveryRttUntilTheRepairDeadline) {
    receiver.set_rtt(20ms);
    render_offset_ms = 400;  // the frame after the gap renders at 1500/90 + 400 = 416 ms
    unlock();
    deliver(receiver, make_rtp(11, 1500, false, {0x61, 0x91}), at(4));
    deliver(receiver, make_rtp(13, 1500, true, {0x61, 0x13}), at(4));
    for (int t = 5; t <= 500; ++t) {
        receiver.tick(at(t));
    }
    // First NACK at 7 ms (reorder grace), then every 20 ms until the
    // deadline 416 + 20 = 436 ms: 7, 27, ..., 427 = 22 NACKs.
    EXPECT_EQ(nacks.size(), 22u);
    for (const auto& n : nacks) EXPECT_EQ(n, std::vector<uint16_t>({12}));
    ASSERT_EQ(losses.size(), 1u) << "a packet past its deadline is lost for real, once";
    EXPECT_EQ(losses[0], 12);
    EXPECT_EQ(receiver.stats().lost_packets, 1u);
    // Its access unit is given up; a late copy is ignored.
    deliver(receiver, make_rtp(12, 1500, false, {0x61, 0x12}), at(510));
    EXPECT_EQ(units.size(), 1u);
    EXPECT_EQ(receiver.stats().late_packets, 1u);
}

// Stage 3: the repair window follows the repairs the link needs. A packet
// that came 400 ms after its gap (after its deadline) makes the next window
// 1.5 x 400 = 600 ms. The receiver before had one fixed deadline, so on a
// link with jitter spikes every late repair was a loss and a key frame.
TEST_F(RtpH264ReceiverTest, ASlowRepairWidensTheRepairWindow) {
    receiver.set_rtt(20ms);
    render_offset_ms = 0;
    unlock();
    EXPECT_EQ(receiver.repair_window(at(4)), 300ms) << "the minimum window";
    deliver(receiver, make_rtp(11, 1500, true, {0x61, 0x91}), at(4));
    deliver(receiver, make_rtp(13, 4500, true, {0x61, 0x93}), at(4));
    for (int t = 5; t <= 303; ++t) receiver.tick(at(t));
    EXPECT_TRUE(losses.empty()) << "the gap seen at 4 ms has until 304 ms";
    for (int t = 304; t <= 310; ++t) receiver.tick(at(t));
    ASSERT_EQ(losses.size(), 1u);
    // The original comes at 404 ms: 400 ms after the gap was seen.
    deliver(receiver, make_rtp(12, 3000, true, {0x61, 0x92}), at(404));
    EXPECT_EQ(receiver.repair_window(at(404)), 600ms);

    // The next gap (seq 15) is seen at 500 ms and is kept until 1100 ms.
    deliver(receiver, make_rtp(14, 6000, true, {0x61, 0x94}), at(500));
    deliver(receiver, make_rtp(16, 9000, true, {0x61, 0x96}), at(500));
    for (int t = 501; t <= 1049; ++t) receiver.tick(at(t));
    EXPECT_EQ(losses.size(), 1u) << "the wider window still waits for the repair";
    deliver(receiver, make_rtp(15, 7500, true, {0x61, 0x95}), at(1050));
    EXPECT_EQ(losses.size(), 1u);
    // Seq 15 completes its access unit; seq 16 waited behind it (its start
    // was unknown while seq 15 could still come).
    ASSERT_GE(units.size(), 2u);
    EXPECT_EQ(units[units.size() - 2].first_sequence, 15);
    EXPECT_TRUE(units[units.size() - 2].retransmitted);
    EXPECT_EQ(units.back().first_sequence, 16);

    // Ten seconds after the last slow repair, the window is the minimum again.
    EXPECT_EQ(receiver.repair_window(at(12'000)), 300ms);
}

// Stage 3: the RTT applies at once, not at a rebuild of the receiver.
TEST_F(RtpH264ReceiverTest, ANewRttChangesTheNackIntervalAtOnce) {
    render_offset_ms = 500;
    unlock();
    deliver(receiver, make_rtp(11, 1500, false, {0x61, 0x91}), at(4));
    deliver(receiver, make_rtp(13, 1500, true, {0x61, 0x13}), at(4));
    for (int t = 5; t <= 50; ++t) receiver.tick(at(t));
    const size_t before = nacks.size();  // 7, 27, 47 at 20 ms
    EXPECT_EQ(before, 3u);
    receiver.set_rtt(50ms);
    for (int t = 51; t <= 150; ++t) receiver.tick(at(t));
    // 97, 147 at 50 ms.
    EXPECT_EQ(nacks.size(), before + 2);
}

// Stage 3: a large key frame that is still arriving is not lost. While
// packets of the frame after a gap keep coming, the deadline moves with them.
TEST_F(RtpH264ReceiverTest, AKeyFrameThatIsStillArrivingIsNotGivenUp) {
    render_offset_ms = 20;
    // IDR at timestamp 0: SPS+PPS STAP, then 60 FU-A fragments, one each 10 ms
    // (a slow link), seq 3 missing until repaired late.
    deliver(receiver, make_rtp(1, 0, false, make_stap({{0x67, 0x42}, {0x68, 0xce}})), at(0));
    deliver(receiver, make_rtp(2, 0, false, {0x7c, 0x85, 0x88}), at(0));
    for (int i = 4; i <= 61; ++i) {
        const bool last = i == 61;
        deliver(receiver, make_rtp(static_cast<uint16_t>(i), 0, last,
                                   {0x7c, static_cast<uint8_t>(last ? 0x45 : 0x05), 0x10}),
                at(10 * i));
        receiver.tick(at(10 * i + 5));
    }
    EXPECT_TRUE(losses.empty()) << "the deadline follows the frame's progress";
    deliver(receiver, make_rtp(3, 0, false, {0x7c, 0x05, 0x33}), at(620));
    ASSERT_EQ(units.size(), 1u);
    EXPECT_TRUE(units[0].keyframe);
    EXPECT_TRUE(units[0].retransmitted);
}

TEST_F(RtpH264ReceiverTest, TwoNewerPacketsEndReorderGrace) {
    unlock();

    deliver(receiver, make_rtp(12, 1500, true, {0x61, 0x12}), at(4));
    EXPECT_TRUE(nacks.empty());
    deliver(receiver, make_rtp(13, 3000, true, {0x61, 0x93}), at(4));

    ASSERT_EQ(nacks.size(), 1u);
    EXPECT_EQ(nacks[0], std::vector<uint16_t>({11}));
}

// Stage 3: the receiver itself never asks for a key frame; a loss goes to
// on_loss once, and only after the repair deadline.
TEST_F(RtpH264ReceiverTest, NothingIsLostBeforeTheDeadline) {
    render_offset_ms = 60;
    unlock();
    deliver(receiver, make_rtp(11, 1500, false, {0x61, 0x91}), at(4));
    deliver(receiver, make_rtp(13, 1500, true, {0x61, 0x13}), at(4));
    // Render deadline 16 + 60 + 20 = 96 ms, but the repair window keeps
    // the gap seen at 4 ms until 304 ms.
    for (int t = 5; t <= 303; ++t) receiver.tick(at(t));
    EXPECT_TRUE(losses.empty());
    receiver.tick(at(305));
    EXPECT_EQ(losses.size(), 1u);
}

TEST_F(RtpH264ReceiverTest, SuppressesDuplicatePackets) {
    unlock();
    deliver(receiver, make_rtp(11, 1500, true, {0x61, 0x91}), at(4));
    deliver(receiver, make_rtp(11, 1500, true, {0x61, 0x91}), at(5));
    deliver(receiver, make_rtp(10, 0, true, idr_payload()), at(5));

    EXPECT_EQ(receiver.stats().duplicates, 2u);
    EXPECT_EQ(units.size(), 2u);
}

// Stage 3: a key frame makes everything before it obsolete: no more NACKs
// for older gaps, no loss for them.
TEST_F(RtpH264ReceiverTest, AKeyFrameEndsTheRepairOfOlderGaps) {
    unlock();
    deliver(receiver, make_rtp(11, 1500, false, {0x61, 0x91}), at(4));
    deliver(receiver, make_rtp(13, 1500, true, {0x61, 0x13}), at(4));
    receiver.tick(at(8));
    ASSERT_EQ(nacks.size(), 1u);
    deliver(receiver, make_rtp(14, 3000, true, idr_payload()), at(10));
    ASSERT_EQ(units.size(), 2u);
    EXPECT_TRUE(units[1].keyframe);
    for (int t = 11; t <= 300; ++t) receiver.tick(at(t));
    EXPECT_EQ(nacks.size(), 1u);
    EXPECT_TRUE(losses.empty());
}

TEST_F(RtpH264ReceiverTest, TracksSequenceWraparound) {
    deliver(receiver, make_rtp(65534, 1000, false, {0x67, 0x01}), at(0));
    deliver(receiver, make_rtp(65535, 1000, false, {0x68, 0x02}), at(0));
    deliver(receiver, make_rtp(0, 1000, true, {0x65, 0x83}), at(0));
    deliver(receiver, make_rtp(1, 2000, true, {0x61, 0x84}), at(1));

    ASSERT_EQ(units.size(), 2u);
    EXPECT_TRUE(units[0].keyframe);
    EXPECT_FALSE(units[1].keyframe);
    EXPECT_EQ(units[1].reference, units[0].last_sequence);
    EXPECT_EQ(receiver.stats().incomplete_access_units, 0u);
    EXPECT_EQ(receiver.stats().extended_highest_sequence, 65537u);
    EXPECT_EQ(receiver.stats().cumulative_loss, 0);
}

TEST_F(RtpH264ReceiverTest, TimestampChangeClosesMarkerlessAccessUnit) {
    deliver(receiver, make_rtp(1, 1000, false, {0x67, 0x01}), at(0));
    deliver(receiver, make_rtp(2, 1000, false, {0x68, 0x02}), at(0));
    deliver(receiver, make_rtp(3, 1000, false, {0x65, 0x83}), at(0));
    deliver(receiver, make_rtp(4, 2000, true, {0x61, 0x84}), at(0));

    ASSERT_EQ(units.size(), 2u);
    EXPECT_EQ(units[0].rtp_timestamp, 1000u);
    EXPECT_EQ(units[1].rtp_timestamp, 2000u);
}

// Stage 3: padding packets of a pacer take sequence numbers but no frame;
// the next frame still references the last frame before them.
TEST_F(RtpH264ReceiverTest, PaddingPacketsDoNotBreakTheReferenceChain) {
    unlock();
    deliver(receiver, make_padding(11, 0), at(2));
    deliver(receiver, make_padding(12, 0), at(3));
    deliver(receiver, make_rtp(13, 1500, true, {0x61, 0x91}), at(16));
    ASSERT_EQ(units.size(), 2u);
    EXPECT_EQ(units[1].reference, 10);
    EXPECT_EQ(receiver.stats().padding_packets, 2u);
    EXPECT_TRUE(nacks.empty());
}

// Stage 3: after a loss, an access unit right behind the lost packet still
// completes when its first NAL can only open an access unit (an IDR).
TEST_F(RtpH264ReceiverTest, AKeyFrameRightAfterALostPacketStillCompletes) {
    render_offset_ms = 0;
    unlock();
    // seq 11 lost; seq 12 is an IDR with SPS first.
    deliver(receiver, make_rtp(12, 3000, true, idr_payload()), at(40));
    EXPECT_EQ(units.size(), 1u) << "the start of seq 12 is unknown while seq 11 may come";
    for (int t = 41; t <= 400; ++t) receiver.tick(at(t));
    ASSERT_EQ(losses.size(), 1u);
    ASSERT_EQ(units.size(), 2u);
    EXPECT_TRUE(units[1].keyframe);
}

TEST_F(RtpH264ReceiverTest, ADiscontinuityIsALoss) {
    unlock();
    deliver(receiver, make_rtp(10 + 5000, 90000, true, {0x61, 0x91}), at(10));
    EXPECT_EQ(receiver.stats().sequence_discontinuities, 1u);
    EXPECT_EQ(losses.size(), 1u);
    EXPECT_TRUE(nacks.empty()) << "a jump is not NACKed packet by packet";
}

TEST_F(RtpH264ReceiverTest, KnowsWhenAKeyFrameIsArriving) {
    deliver(receiver, make_rtp(1, 0, false, make_stap({{0x67, 0x42}, {0x68, 0xce}})), at(0));
    deliver(receiver, make_rtp(2, 0, false, {0x7c, 0x85, 0x88}), at(10));
    EXPECT_TRUE(receiver.keyframe_in_progress(at(100)));
    EXPECT_FALSE(receiver.keyframe_in_progress(at(400)));
}

TEST_F(RtpH264ReceiverTest, RejectsInvalidRtpAndH264Payloads) {
    receiver.on_rtp_packet(nullptr, 0, at(0));

    std::vector<uint8_t> short_packet(11, 0);
    receiver.on_rtp_packet(short_packet.data(), short_packet.size(), at(0));

    auto wrong_version = make_rtp(1, 1, true, {0x61, 0x01});
    wrong_version[0] = 0x40;
    deliver(receiver, std::move(wrong_version), at(0));

    std::vector<uint8_t> bad_extension(16, 0);
    bad_extension[0] = 0x90;
    bad_extension[1] = 96;
    bad_extension[14] = 0;
    bad_extension[15] = 1;
    deliver(receiver, std::move(bad_extension), at(0));

    auto wrong_pt = make_rtp(1, 1, true, {0x61, 0x01});
    wrong_pt[1] = static_cast<uint8_t>(0x80 | 97);
    deliver(receiver, std::move(wrong_pt), at(0));

    deliver(receiver, make_rtp(1, 1, true, {0x78, 0x00, 0x04, 0x67}), at(0));
    deliver(receiver, make_rtp(2, 1, true, {0x7c, 0xc1, 0xaa}), at(0));
    deliver(receiver, make_rtp(3, 1, true, {0x79, 0xaa}), at(0));

    const auto stats = receiver.stats();
    EXPECT_EQ(stats.invalid_rtp_packets, 4u);
    EXPECT_EQ(stats.wrong_payload_type_packets, 1u);
    EXPECT_EQ(stats.invalid_h264_packets, 3u);
    EXPECT_TRUE(units.empty());
}

TEST(RtpH264ReceiverBoundsTest, PacketMemoryStaysWithinItsBounds) {
    RtpH264Receiver receiver;
    std::vector<uint8_t> large_nal(16 * 1024, 0x55);
    large_nal[0] = 0x61;
    // Never complete: no marker, one timestamp, a slice that cannot open an
    // access unit after packet 0.
    for (uint32_t sequence = 0; sequence <= 1200; ++sequence) {
        deliver(receiver, make_rtp(static_cast<uint16_t>(sequence), 500, false, large_nal), at(0));
    }
    const auto stats = receiver.stats();
    EXPECT_LE(stats.buffered_packets, RtpH264Receiver::kMaxPackets);
    EXPECT_LE(stats.peak_buffered_bytes, RtpH264Receiver::kMaxBufferedBytes + large_nal.size());
    EXPECT_LE(stats.buffered_bytes, RtpH264Receiver::kMaxBufferedBytes);
    EXPECT_GE(stats.buffer_evictions, 1u);
}

TEST(RtpH264ReceiverStartTest, OnlyAnAccessUnitStartOpensAfterAGap) {
    using mello::transport::h264_payload_starts_access_unit;
    const std::vector<uint8_t> sps{0x67, 0x42};
    const std::vector<uint8_t> pps{0x68, 0xce};
    const std::vector<uint8_t> slice_mb0{0x61, 0x88};
    const std::vector<uint8_t> slice_mb5{0x61, 0x28};
    const std::vector<uint8_t> fu_start_mb0{0x7c, 0x85, 0x88};
    const std::vector<uint8_t> fu_middle{0x7c, 0x05, 0x88};
    const auto stap = make_stap({{0x67, 0x42}, {0x68, 0xce}});
    EXPECT_TRUE(h264_payload_starts_access_unit(sps.data(), sps.size()));
    EXPECT_FALSE(h264_payload_starts_access_unit(pps.data(), pps.size()));
    EXPECT_TRUE(h264_payload_starts_access_unit(slice_mb0.data(), slice_mb0.size()));
    EXPECT_FALSE(h264_payload_starts_access_unit(slice_mb5.data(), slice_mb5.size()));
    EXPECT_TRUE(h264_payload_starts_access_unit(fu_start_mb0.data(), fu_start_mb0.size()));
    EXPECT_FALSE(h264_payload_starts_access_unit(fu_middle.data(), fu_middle.size()));
    EXPECT_TRUE(h264_payload_starts_access_unit(stap.data(), stap.size()));
}

} // namespace
