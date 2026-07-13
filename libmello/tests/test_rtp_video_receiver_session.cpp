#include <gtest/gtest.h>

#include "transport/rtp_video_receiver_session.hpp"

#include <rtc/rtp.hpp>

#include <cstdint>
#include <vector>

namespace {

using mello::transport::detail::compress_generic_nack_sequences;
using mello::transport::detail::make_generic_nack_packet;
using mello::transport::detail::make_pli_packet;

TEST(RtpVideoReceiverSessionRtcpTest, CompressesPidBlpAcrossSequenceWrap) {
    const auto blocks = compress_generic_nack_sequences(
        {17, 0, 65'535, 18, 1, 0}
    );

    ASSERT_EQ(blocks.size(), 2u);
    EXPECT_EQ(blocks[0].pid, 65'535);
    EXPECT_EQ(blocks[0].blp, 0x0003);
    EXPECT_EQ(blocks[1].pid, 17);
    EXPECT_EQ(blocks[1].blp, 0x0001);
}

TEST(RtpVideoReceiverSessionRtcpTest, BuildsValidGenericNackHeaderAndFci) {
    constexpr uint32_t sender_ssrc = 0x11223344;
    constexpr uint32_t media_ssrc = 0xaabbccdd;
    auto packet = make_generic_nack_packet(
        sender_ssrc,
        media_ssrc,
        {100, 101, 116, 117}
    );

    ASSERT_EQ(packet.size(), rtc::RtcpNack::Size(2));
    auto* const nack = reinterpret_cast<rtc::RtcpNack*>(packet.data());
    EXPECT_EQ(nack->header.header.version(), 2);
    EXPECT_EQ(nack->header.header.payloadType(), 205);
    EXPECT_EQ(nack->header.header.reportCount(), 1);
    EXPECT_EQ(nack->header.header.lengthInBytes(), packet.size());
    EXPECT_EQ(nack->header.packetSenderSSRC(), sender_ssrc);
    EXPECT_EQ(nack->header.mediaSourceSSRC(), media_ssrc);
    EXPECT_EQ(nack->parts[0].pid(), 100);
    EXPECT_EQ(nack->parts[0].blp(), 0x8001);
    EXPECT_EQ(nack->parts[1].pid(), 117);
    EXPECT_EQ(nack->parts[1].blp(), 0);
}

TEST(RtpVideoReceiverSessionRtcpTest, BuildsValidPliWithDistinctSsrcs) {
    constexpr uint32_t sender_ssrc = 0x01020304;
    constexpr uint32_t media_ssrc = 0xf1f2f3f4;
    const auto packet = make_pli_packet(sender_ssrc, media_ssrc);

    ASSERT_EQ(packet.size(), rtc::RtcpPli::Size());
    const auto* const pli =
        reinterpret_cast<const rtc::RtcpPli*>(packet.data());
    EXPECT_EQ(pli->header.header.version(), 2);
    EXPECT_EQ(pli->header.header.payloadType(), 206);
    EXPECT_EQ(pli->header.header.reportCount(), 1);
    EXPECT_EQ(pli->header.header.lengthInBytes(), packet.size());
    EXPECT_EQ(pli->header.packetSenderSSRC(), sender_ssrc);
    EXPECT_EQ(pli->header.mediaSourceSSRC(), media_ssrc);
}

} // namespace
