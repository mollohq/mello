#include <gtest/gtest.h>

#include "transport/ulpfec.hpp"

#include <cstdint>
#include <initializer_list>
#include <vector>

using mello::transport::UlpfecGenerator;
using mello::transport::UlpfecRecovery;
using mello::transport::kUlpfecPayloadType;

namespace {

constexpr uint32_t kMediaSsrc = 0x11223344;

uint16_t read_u16_be(const uint8_t* data) {
    return static_cast<uint16_t>(
        (static_cast<uint16_t>(data[0]) << 8) | data[1]
    );
}

std::vector<uint8_t> make_media(
    uint16_t sequence,
    uint32_t timestamp,
    bool marker,
    uint8_t fill,
    size_t payload_bytes
) {
    std::vector<uint8_t> packet(12 + payload_bytes);
    packet[0] = 0x80;
    packet[1] = static_cast<uint8_t>((marker ? 0x80 : 0) | 96);
    packet[2] = static_cast<uint8_t>(sequence >> 8);
    packet[3] = static_cast<uint8_t>(sequence & 0xff);
    packet[4] = static_cast<uint8_t>(timestamp >> 24);
    packet[5] = static_cast<uint8_t>(timestamp >> 16);
    packet[6] = static_cast<uint8_t>(timestamp >> 8);
    packet[7] = static_cast<uint8_t>(timestamp & 0xff);
    packet[8] = static_cast<uint8_t>(kMediaSsrc >> 24);
    packet[9] = static_cast<uint8_t>(kMediaSsrc >> 16);
    packet[10] = static_cast<uint8_t>(kMediaSsrc >> 8);
    packet[11] = static_cast<uint8_t>(kMediaSsrc & 0xff);
    for (size_t i = 12; i < packet.size(); ++i) {
        packet[i] = fill;
    }
    return packet;
}

TEST(UlpfecGeneratorTest, EmitsOnePacketPerCompleteGroup) {
    UlpfecGenerator generator(10);
    std::vector<uint8_t> fec;
    size_t completions = 0;
    for (uint16_t seq = 100; seq < 110; ++seq) {
        auto packet = make_media(seq, 90'000, seq == 109, 0x55, 500);
        generator.add_packet(packet.data(), packet.size());
        if (generator.pending() == 0) {
            ++completions;
            fec = generator.build_packet(kMediaSsrc, 90'000);
        }
    }
    EXPECT_EQ(completions, 1u);

    ASSERT_FALSE(fec.empty());
    EXPECT_EQ(fec[1] & 0x7f, kUlpfecPayloadType);
    EXPECT_EQ(static_cast<unsigned>(fec[1] & 0x80), 0x80u); // marker of last protected packet
    // SSRC = media + 1
    const uint32_t ssrc = (uint32_t{fec[8]} << 24) | (uint32_t{fec[9]} << 16)
        | (uint32_t{fec[10]} << 8) | fec[11];
    EXPECT_EQ(ssrc, kMediaSsrc + 1);
    // ULPFEC header: SN base = 100, mask = 10 leading bits set.
    const uint16_t sn_base =
        static_cast<uint16_t>((fec[14] << 8) | fec[15]);
    EXPECT_EQ(sn_base, 100);
    const uint16_t mask = static_cast<uint16_t>((fec[22] << 8) | fec[23]);
    EXPECT_EQ(mask, 0xffc0u);
}

TEST(UlpfecRecoveryTest, ReconstructsSingleLossBitExact) {
    UlpfecGenerator generator(10);
    std::vector<std::vector<uint8_t>> media;
    for (uint16_t seq = 0; seq < 10; ++seq) {
        media.push_back(make_media(
            seq,
            45'000 + seq * 3'000,
            seq == 9,
            static_cast<uint8_t>(0x30 + seq),
            200 + seq * 37));
    }
    std::vector<uint8_t> fec;
    for (const auto& packet : media) {
        generator.add_packet(packet.data(), packet.size());
    }
    ASSERT_EQ(generator.pending(), 0u);
    fec = generator.build_packet(kMediaSsrc, 45'000 + 9 * 3'000);
    ASSERT_FALSE(fec.empty());

    UlpfecRecovery recovery;
    for (size_t i = 0; i < media.size(); ++i) {
        if (i == 4) continue; // "lost"
        recovery.add_media_packet(media[i].data(), media[i].size());
    }
    recovery.add_fec_packet(fec.data(), fec.size());

    std::vector<uint8_t> out;
    ASSERT_TRUE(recovery.recover(4, out));
    EXPECT_EQ(out, media[4]);

    uint64_t recovered = 0;
    uint64_t unrecoverable = 0;
    recovery.stats(recovered, unrecoverable);
    EXPECT_EQ(recovered, 1u);
    EXPECT_EQ(unrecoverable, 0u);

    // A second recover of the same sequence is already cached (no dup).
    EXPECT_TRUE(recovery.uncovered_mask_sequences().empty());
}

TEST(UlpfecRecoveryTest, TwoLossesInOneGroupAreUnrecoverable) {
    UlpfecGenerator generator(10);
    for (uint16_t seq = 50; seq < 60; ++seq) {
        auto packet = make_media(seq, 60'000, seq == 59, 0x77, 300);
        generator.add_packet(packet.data(), packet.size());
    }
    const auto fec = generator.build_packet(kMediaSsrc, 60'000);

    UlpfecRecovery recovery;
    for (uint16_t seq = 50; seq < 60; ++seq) {
        if (seq == 53 || seq == 57) continue;
        auto packet = make_media(seq, 60'000, seq == 59, 0x77, 300);
        recovery.add_media_packet(packet.data(), packet.size());
    }
    recovery.add_fec_packet(fec.data(), fec.size());

    std::vector<uint8_t> out;
    EXPECT_FALSE(recovery.recover(53, out));
    EXPECT_FALSE(recovery.recover(57, out));

    uint64_t recovered = 0;
    uint64_t unrecoverable = 0;
    uint64_t pending = 0;
    recovery.stats(recovered, unrecoverable, pending);
    EXPECT_EQ(unrecoverable, 0u);
    EXPECT_EQ(pending, 2u);
}

TEST(UlpfecRecoveryTest, RecoverIncrementsOncePerSequence) {
    UlpfecGenerator generator(4);
    for (uint16_t seq = 40; seq < 44; ++seq) {
        auto packet = make_media(seq, 50'000, seq == 43, 0x55, 80);
        generator.add_packet(packet.data(), packet.size());
    }
    const auto fec = generator.build_packet(kMediaSsrc, 50'000);

    UlpfecRecovery recovery;
    for (uint16_t seq = 40; seq < 44; ++seq) {
        if (seq == 41) {
            continue;
        }
        auto packet = make_media(seq, 50'000, seq == 43, 0x55, 80);
        recovery.add_media_packet(packet.data(), packet.size());
    }
    recovery.add_fec_packet(fec.data(), fec.size());

    std::vector<uint8_t> first;
    std::vector<uint8_t> second;
    ASSERT_TRUE(recovery.recover(41, first));
    EXPECT_FALSE(recovery.recover(41, second));

    uint64_t recovered = 0;
    uint64_t unrecoverable = 0;
    recovery.stats(recovered, unrecoverable);
    EXPECT_EQ(recovered, 1u);
}

TEST(UlpfecGeneratorTest, NonContiguousGroupEmitsNothing) {
    UlpfecGenerator generator(4);
    for (const uint16_t seq :
         std::initializer_list<uint16_t>{10, 11, 13, 14}) { // gap at 12
        auto packet = make_media(seq, 10'000, false, 0x11, 100);
        generator.add_packet(packet.data(), packet.size());
    }
    const auto fec = generator.build_packet(kMediaSsrc, 10'000);
    EXPECT_TRUE(fec.empty());
}

TEST(UlpfecRecoveryTest, WorksAcrossGroupBoundary) {
    UlpfecGenerator generator(4);
    UlpfecRecovery recovery;
    // Group A: seqs 20..23, group B: seqs 24..27; lose 22 (in A) and 25 (in B).
    for (uint16_t seq = 20; seq < 28; ++seq) {
        auto packet = make_media(seq, seq * 1'000, (seq % 4) == 3, 0x42, 128);
        generator.add_packet(packet.data(), packet.size());
        if (generator.pending() == 0) {
            const auto fec = generator.build_packet(kMediaSsrc, seq * 1'000);
            recovery.add_fec_packet(fec.data(), fec.size());
        }
        if (seq != 22 && seq != 25) {
            recovery.add_media_packet(packet.data(), packet.size());
        }
    }

    std::vector<uint8_t> out;
    ASSERT_TRUE(recovery.recover(22, out));
    EXPECT_EQ(out, make_media(22, 22'000, false, 0x42, 128));
    ASSERT_TRUE(recovery.recover(25, out));
    EXPECT_EQ(out, make_media(25, 25'000, false, 0x42, 128));

    uint64_t recovered = 0;
    uint64_t unrecoverable = 0;
    recovery.stats(recovered, unrecoverable);
    EXPECT_EQ(recovered, 2u);
}

TEST(UlpfecRecoveryTest, RecoveredMarkerWhenTailIsNotHighestInGroup) {
    UlpfecGenerator generator(4);
    // AU tail at seq 102 (marker); seq 103 starts the next AU (new timestamp).
    for (uint16_t seq = 100; seq <= 103; ++seq) {
        const uint32_t timestamp = seq <= 102 ? 90'000u : 91'000u;
        auto packet = make_media(
            seq,
            timestamp,
            seq == 102,
            static_cast<uint8_t>(seq),
            64
        );
        generator.add_packet(packet.data(), packet.size());
    }
    const auto fec = generator.build_packet(kMediaSsrc, 91'000);

    UlpfecRecovery recovery;
    for (uint16_t seq = 100; seq <= 103; ++seq) {
        if (seq == 102) {
            continue;
        }
        const uint32_t timestamp = seq <= 102 ? 90'000u : 91'000u;
        auto packet = make_media(
            seq,
            timestamp,
            seq == 102,
            static_cast<uint8_t>(seq),
            64
        );
        recovery.add_media_packet(packet.data(), packet.size());
    }
    recovery.add_fec_packet(fec.data(), fec.size());

    std::vector<uint8_t> out;
    ASSERT_TRUE(recovery.recover(102, out));
    EXPECT_NE(static_cast<unsigned>(out[1] & 0x80), 0u);
    EXPECT_EQ(read_u16_be(out.data() + 2), 102u);
}

} // namespace
