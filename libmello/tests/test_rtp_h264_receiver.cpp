#include <gtest/gtest.h>

#include "transport/rtp_h264_receiver.hpp"

#include <chrono>
#include <cstdint>
#include <initializer_list>
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

struct EmittedFrame {
    std::vector<uint8_t> bytes;
    bool is_idr = false;
    uint32_t timestamp = 0;
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
    EXPECT_LE(options.csrc_count, 15);
    EXPECT_EQ(options.extension.size() % 4, 0u);

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

void deliver(RtpH264Receiver& receiver,
             std::vector<uint8_t> packet,
             RtpH264Receiver::TimePoint now) {
    receiver.on_rtp_packet(packet.data(), packet.size(), now);
}

class RtpH264ReceiverTest : public ::testing::Test {
protected:
    RtpH264ReceiverTest()
        : receiver(RtpH264Receiver::Callbacks{
              [this](const std::vector<uint8_t>& bytes,
                     bool is_idr,
                     uint32_t timestamp) {
                  frames.push_back({bytes, is_idr, timestamp});
              },
              [this](const std::vector<uint16_t>& sequences) {
                  nacks.push_back(sequences);
              },
              [this]() { ++plis; },
          }) {}

    void unlock(uint16_t sequence = 10, int start_ms = 0) {
        const auto payload = make_stap(
            {{0x67, 0x42}, {0x68, 0xce}, {0x65, 0xaa}});
        deliver(receiver,
                make_rtp(sequence, 1000, true, payload),
                at(start_ms));
        receiver.tick(at(start_ms + 3));
        ASSERT_FALSE(receiver.gated());
        ASSERT_EQ(frames.size(), 1u);
    }

    std::vector<EmittedFrame> frames;
    std::vector<std::vector<uint16_t>> nacks;
    int plis = 0;
    RtpH264Receiver receiver;
};

TEST_F(RtpH264ReceiverTest, ReconstructsZeroLossAccessUnit) {
    RtpOptions options;
    options.csrc_count = 2;
    options.extension = {0x10, 0x20, 0x30, 0x40};
    options.padding = 4;

    deliver(receiver,
            make_rtp(100, 9000, false, {0x67, 0x42}, options),
            at(0));
    deliver(receiver,
            make_rtp(101, 9000, false, {0x68, 0xce}),
            at(0));
    deliver(receiver,
            make_rtp(102, 9000, true, {0x65, 0xaa, 0xbb}),
            at(0));

    ASSERT_EQ(frames.size(), 1u);
    EXPECT_EQ(frames[0].bytes,
              annex_b({{0x67, 0x42},
                       {0x68, 0xce},
                       {0x65, 0xaa, 0xbb}}));
    EXPECT_TRUE(frames[0].is_idr);
    EXPECT_EQ(frames[0].timestamp, 9000u);
    EXPECT_FALSE(receiver.gated());

    const auto stats = receiver.stats();
    EXPECT_EQ(stats.packets, 3u);
    EXPECT_EQ(stats.accepted_packets, 3u);
    EXPECT_EQ(stats.complete_access_units, 1u);
    EXPECT_EQ(stats.emitted_access_units, 1u);
}

TEST(RtpH264ReceiverConfigurationTest, AcceptsConfiguredPayloadType) {
    std::vector<EmittedFrame> frames;
    RtpH264Receiver::Callbacks callbacks;
    callbacks.on_access_unit =
        [&frames](const std::vector<uint8_t>& bytes,
                  bool is_idr,
                  uint32_t timestamp) {
            frames.push_back({bytes, is_idr, timestamp});
        };
    RtpH264Receiver::Config config;
    config.payload_type = 110;
    RtpH264Receiver configured(std::move(callbacks), config);

    RtpOptions options;
    options.payload_type = 110;
    const auto stap = make_stap(
        {{0x67, 0x01}, {0x68, 0x02}, {0x65, 0x03}});
    deliver(configured, make_rtp(1, 1234, true, stap, options), at(0));
    configured.tick(at(3));

    ASSERT_EQ(frames.size(), 1u);
    EXPECT_EQ(frames[0].timestamp, 1234u);
    EXPECT_EQ(configured.stats().wrong_payload_type_packets, 0u);
}

TEST(RtpH264ReceiverSourceTest, LearnsAndStrictlyFiltersSsrc) {
    RtpH264Receiver receiver;
    RtpOptions source;
    source.ssrc = 0x11223344;
    RtpOptions other_source;
    other_source.ssrc = 0x55667788;

    const auto first =
        make_rtp(1, 1000, false, {0x61, 0x01}, source);
    const auto foreign =
        make_rtp(2, 1000, true, {0x61, 0x02}, other_source);
    const auto second =
        make_rtp(2, 1000, true, {0x61, 0x02}, source);
    const size_t received_bytes =
        first.size() + foreign.size() + second.size();
    const size_t accepted_bytes = first.size() + second.size();

    deliver(receiver, first, at(0));
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

    deliver(receiver,
            make_rtp(1, 1000, true, {0x79, 0x01}, invalid_source),
            at(0));
    deliver(receiver,
            make_rtp(1, 1000, true, {0x61, 0x01}, valid_source),
            at(1));

    const auto stats = receiver.stats();
    EXPECT_TRUE(stats.has_ssrc);
    EXPECT_EQ(stats.ssrc, valid_source.ssrc);
    EXPECT_EQ(stats.invalid_h264_packets, 1u);
    EXPECT_EQ(stats.wrong_ssrc_packets, 0u);
    EXPECT_EQ(stats.accepted_packets, 1u);
}

TEST(RtpH264ReceiverTimeTest, RejectsBackwardsInputsWithoutMovingDeadlines) {
    std::vector<std::vector<uint16_t>> nacks;
    RtpH264Receiver::Callbacks callbacks;
    callbacks.on_nack =
        [&nacks](const std::vector<uint16_t>& sequences) {
            nacks.push_back(sequences);
        };
    RtpH264Receiver receiver(std::move(callbacks));

    deliver(receiver,
            make_rtp(1, 1000, false, {0x61, 0x01}),
            at(10));
    deliver(receiver,
            make_rtp(3, 1000, true, {0x61, 0x03}),
            at(10));
    deliver(receiver,
            make_rtp(2, 1000, false, {0x61, 0x02}),
            at(9));
    receiver.tick(at(8));

    EXPECT_TRUE(nacks.empty());
    EXPECT_EQ(receiver.stats().backwards_time_inputs, 2u);
    EXPECT_EQ(receiver.stats().accepted_packets, 2u);
    EXPECT_EQ(receiver.stats().repaired_packets, 0u);

    receiver.tick(at(13));
    ASSERT_EQ(nacks.size(), 1u);
    EXPECT_EQ(nacks[0], std::vector<uint16_t>({2}));
}

TEST(RtpH264ReceiverStatsTest, ComputesRfc3550InterarrivalJitter) {
    RtpH264Receiver receiver;

    deliver(receiver,
            make_rtp(1, 0, true, {0x61, 0x01}),
            at(0));
    deliver(receiver,
            make_rtp(2, 900, true, {0x61, 0x02}),
            at(10));
    EXPECT_EQ(receiver.stats().interarrival_jitter, 0u);

    deliver(receiver,
            make_rtp(3, 1800, true, {0x61, 0x03}),
            at(30));
    EXPECT_EQ(receiver.stats().interarrival_jitter, 56u);

    deliver(receiver,
            make_rtp(4, 2700, true, {0x61, 0x04}),
            at(40));
    EXPECT_EQ(receiver.stats().interarrival_jitter, 52u);
}

TEST_F(RtpH264ReceiverTest, ReassemblesFuA) {
    unlock();

    deliver(receiver,
            make_rtp(11, 2000, false, {0x7c, 0x81, 0xaa}),
            at(4));
    deliver(receiver,
            make_rtp(12, 2000, false, {0x7c, 0x01, 0xbb}),
            at(4));
    deliver(receiver,
            make_rtp(13, 2000, true, {0x7c, 0x41, 0xcc}),
            at(4));

    ASSERT_EQ(frames.size(), 2u);
    EXPECT_EQ(frames[1].bytes, annex_b({{0x61, 0xaa, 0xbb, 0xcc}}));
    EXPECT_FALSE(frames[1].is_idr);
    EXPECT_TRUE(nacks.empty());
}

TEST_F(RtpH264ReceiverTest, UnpacksStapA) {
    const auto stap = make_stap(
        {{0x67, 0x01}, {0x68, 0x02}, {0x65, 0x03}});
    deliver(receiver, make_rtp(20, 3000, true, stap), at(0));

    EXPECT_TRUE(frames.empty());
    receiver.tick(at(3));

    ASSERT_EQ(frames.size(), 1u);
    EXPECT_EQ(frames[0].bytes,
              annex_b({{0x67, 0x01},
                       {0x68, 0x02},
                       {0x65, 0x03}}));
    EXPECT_TRUE(frames[0].is_idr);
}

TEST_F(RtpH264ReceiverTest, ReordersBeforeEmitting) {
    unlock();

    deliver(receiver,
            make_rtp(12, 2000, true, {0x61, 0x12}),
            at(4));
    EXPECT_EQ(frames.size(), 1u);
    deliver(receiver,
            make_rtp(11, 2000, false, {0x61, 0x11}),
            at(5));

    ASSERT_EQ(frames.size(), 2u);
    EXPECT_EQ(frames[1].bytes,
              annex_b({{0x61, 0x11}, {0x61, 0x12}}));
    EXPECT_TRUE(nacks.empty());
}

TEST_F(RtpH264ReceiverTest, NacksAndRepairsOnePacketLoss) {
    unlock();

    deliver(receiver,
            make_rtp(11, 2000, false, {0x7c, 0x81, 0xaa}),
            at(4));
    deliver(receiver,
            make_rtp(13, 2000, true, {0x7c, 0x41, 0xcc}),
            at(4));
    receiver.tick(at(7));

    ASSERT_EQ(nacks.size(), 1u);
    EXPECT_EQ(nacks[0], std::vector<uint16_t>({12}));
    EXPECT_EQ(frames.size(), 1u);

    deliver(receiver,
            make_rtp(12, 2000, false, {0x7c, 0x01, 0xbb}),
            at(8));

    ASSERT_EQ(frames.size(), 2u);
    EXPECT_EQ(frames[1].bytes, annex_b({{0x61, 0xaa, 0xbb, 0xcc}}));
    const auto stats = receiver.stats();
    EXPECT_EQ(stats.incomplete_access_units, 0u);
    EXPECT_EQ(stats.missing_sequences_detected, 1u);
    EXPECT_EQ(stats.nacks, 1u);
    EXPECT_EQ(stats.repaired_packets, 1u);
    EXPECT_EQ(stats.cumulative_loss, 0);
}

TEST_F(RtpH264ReceiverTest, RepeatsNackOnlyOnce) {
    unlock();

    deliver(receiver,
            make_rtp(11, 2000, false, {0x61, 0x11}),
            at(4));
    deliver(receiver,
            make_rtp(13, 2000, true, {0x61, 0x13}),
            at(4));
    receiver.tick(at(7));
    receiver.tick(at(22));
    receiver.tick(at(38));

    ASSERT_EQ(nacks.size(), 2u);
    EXPECT_EQ(nacks[0], std::vector<uint16_t>({12}));
    EXPECT_EQ(nacks[1], std::vector<uint16_t>({12}));
    EXPECT_EQ(receiver.stats().nacks, 2u);
    EXPECT_EQ(receiver.stats().missing_sequences_detected, 1u);
}

TEST_F(RtpH264ReceiverTest, TwoNewerPacketsEndReorderGrace) {
    unlock();

    deliver(receiver,
            make_rtp(12, 2000, true, {0x61, 0x12}),
            at(4));
    EXPECT_TRUE(nacks.empty());
    deliver(receiver,
            make_rtp(13, 3000, true, {0x61, 0x13}),
            at(4));

    ASSERT_EQ(nacks.size(), 1u);
    EXPECT_EQ(nacks[0], std::vector<uint16_t>({11}));
}

TEST_F(RtpH264ReceiverTest, ExpiryRequestsPliAndRegates) {
    unlock();

    deliver(receiver,
            make_rtp(11, 2000, false, {0x7c, 0x81, 0xaa}),
            at(4));
    deliver(receiver,
            make_rtp(13, 2000, true, {0x7c, 0x41, 0xcc}),
            at(4));
    receiver.tick(at(125));

    EXPECT_TRUE(receiver.gated());
    EXPECT_EQ(plis, 1);
    EXPECT_EQ(receiver.stats().incomplete_access_units, 1u);

    deliver(receiver,
            make_rtp(14, 3000, true, {0x61, 0xdd}),
            at(126));
    EXPECT_EQ(frames.size(), 1u);
    EXPECT_EQ(receiver.stats().gate_dropped_access_units, 1u);
    EXPECT_EQ(plis, 1) << "PLI cooldown must suppress a storm";

    const auto recovery = make_stap(
        {{0x67, 0x10}, {0x68, 0x20}, {0x65, 0x30}});
    deliver(receiver,
            make_rtp(15, 4000, true, recovery),
            at(127));

    EXPECT_FALSE(receiver.gated());
    ASSERT_EQ(frames.size(), 2u);
    EXPECT_TRUE(frames[1].is_idr);
    EXPECT_EQ(receiver.stats().gate_entries, 2u);
    EXPECT_EQ(receiver.stats().gate_exits, 2u);
}

TEST_F(RtpH264ReceiverTest, SuppressesLateAndDuplicatePackets) {
    unlock();

    deliver(receiver,
            make_rtp(10, 1000, true, {0x65, 0xff}),
            at(4));
    deliver(receiver,
            make_rtp(11, 2000, true, {0x61, 0x11}),
            at(4));
    deliver(receiver,
            make_rtp(9, 900, true, {0x61, 0x09}),
            at(5));
    deliver(receiver,
            make_rtp(9, 900, true, {0x61, 0x09}),
            at(5));

    const auto stats = receiver.stats();
    EXPECT_EQ(stats.duplicates, 2u);
    EXPECT_EQ(stats.late_packets, 1u);
    EXPECT_EQ(frames.size(), 2u);
}

TEST_F(RtpH264ReceiverTest, TracksSequenceWraparound) {
    deliver(receiver,
            make_rtp(65534, 1000, false, {0x67, 0x01}),
            at(0));
    deliver(receiver,
            make_rtp(65535, 1000, false, {0x68, 0x02}),
            at(0));
    deliver(receiver,
            make_rtp(0, 1000, true, {0x65, 0x03}),
            at(0));
    deliver(receiver,
            make_rtp(1, 2000, true, {0x61, 0x04}),
            at(1));

    ASSERT_EQ(frames.size(), 2u);
    EXPECT_TRUE(frames[0].is_idr);
    EXPECT_FALSE(frames[1].is_idr);
    EXPECT_EQ(receiver.stats().late_packets, 0u);
    EXPECT_EQ(receiver.stats().incomplete_access_units, 0u);
    EXPECT_EQ(receiver.stats().extended_highest_sequence, 65537u);
    EXPECT_EQ(receiver.stats().cumulative_loss, 0);
}

TEST_F(RtpH264ReceiverTest, TimestampChangeClosesMarkerlessAccessUnit) {
    deliver(receiver,
            make_rtp(1, 1000, false, {0x67, 0x01}),
            at(0));
    deliver(receiver,
            make_rtp(2, 1000, false, {0x68, 0x02}),
            at(0));
    deliver(receiver,
            make_rtp(3, 1000, false, {0x65, 0x03}),
            at(0));
    deliver(receiver,
            make_rtp(4, 2000, true, {0x61, 0x04}),
            at(0));

    ASSERT_EQ(frames.size(), 2u);
    EXPECT_EQ(frames[0].timestamp, 1000u);
    EXPECT_EQ(frames[1].timestamp, 2000u);
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

    deliver(receiver,
            make_rtp(1, 1, true, {0x78, 0x00, 0x04, 0x67}),
            at(0));
    deliver(receiver,
            make_rtp(2, 1, true, {0x7c, 0xc1, 0xaa}),
            at(0));
    deliver(receiver,
            make_rtp(3, 1, true, {0x79, 0xaa}),
            at(0));

    const auto stats = receiver.stats();
    EXPECT_EQ(stats.invalid_rtp_packets, 4u);
    EXPECT_EQ(stats.wrong_payload_type_packets, 1u);
    EXPECT_EQ(stats.invalid_h264_packets, 3u);
    EXPECT_TRUE(frames.empty());
}

TEST(RtpH264ReceiverBoundsTest, EnforcesAccessUnitPacketAndByteLimits) {
    RtpH264Receiver access_unit_receiver;
    deliver(access_unit_receiver,
            make_rtp(1, 100, false, {0x61, 0x01}),
            at(0));
    deliver(access_unit_receiver,
            make_rtp(3, 200, false, {0x61, 0x03}),
            at(0));
    deliver(access_unit_receiver,
            make_rtp(5, 300, false, {0x61, 0x05}),
            at(0));
    deliver(access_unit_receiver,
            make_rtp(7, 400, false, {0x61, 0x07}),
            at(0));

    const auto access_unit_stats = access_unit_receiver.stats();
    EXPECT_LE(access_unit_stats.buffered_access_units,
              RtpH264Receiver::kMaxAccessUnits);
    EXPECT_LE(access_unit_stats.peak_buffered_access_units,
              RtpH264Receiver::kMaxAccessUnits);
    EXPECT_GE(access_unit_stats.buffer_evictions, 1u);

    RtpH264Receiver packet_receiver;
    std::vector<uint8_t> large_nal(4096, 0x55);
    large_nal[0] = 0x61;
    for (uint16_t sequence = 0; sequence <= 256; ++sequence) {
        deliver(packet_receiver,
                make_rtp(sequence, 500, false, large_nal),
                at(0));
    }

    const auto packet_stats = packet_receiver.stats();
    EXPECT_LE(packet_stats.buffered_packets,
              RtpH264Receiver::kMaxPackets);
    EXPECT_LE(packet_stats.peak_buffered_packets,
              RtpH264Receiver::kMaxPackets);
    EXPECT_LE(packet_stats.buffered_bytes,
              RtpH264Receiver::kMaxBufferedBytes);
    EXPECT_LE(packet_stats.peak_buffered_bytes,
              RtpH264Receiver::kMaxBufferedBytes);
    EXPECT_GE(packet_stats.buffer_evictions, 1u);
}

TEST(RtpH264ReceiverConfigTest, AdaptiveNackBudgetAllowsMoreRetries) {
    std::vector<std::vector<uint16_t>> nacks;
    RtpH264Receiver::Config config;
    config.nack_max_attempts = 5;
    RtpH264Receiver receiver(
        RtpH264Receiver::Callbacks{
            [](const std::vector<uint8_t>&, bool, uint32_t) {},
            [&nacks](const std::vector<uint16_t>& sequences) {
                nacks.push_back(sequences);
            },
            []() {},
        },
        config
    );

    // Open the gate with an IDR AU, then leave sequence 12 missing.
    deliver(receiver,
            make_rtp(10,
                     1000,
                     true,
                     make_stap({{0x67, 0x42}, {0x68, 0xce}, {0x65, 0xaa}})),
            at(0));
    receiver.tick(at(3));
    deliver(receiver, make_rtp(11, 2000, false, {0x61, 0x11}), at(4));
    deliver(receiver, make_rtp(13, 2000, true, {0x61, 0x13}), at(4));

    receiver.tick(at(7));
    receiver.tick(at(22));
    receiver.tick(at(37));
    receiver.tick(at(52));
    receiver.tick(at(67));
    receiver.tick(at(82));

    // Default budget is 2 attempts; the configured budget allows 5.
    EXPECT_EQ(nacks.size(), 5u);
    EXPECT_EQ(receiver.stats().nacks, 5u);
    EXPECT_EQ(receiver.stats().missing_sequences_detected, 1u);
}

} // namespace
