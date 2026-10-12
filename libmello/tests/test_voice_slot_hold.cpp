// Voice receive slots: RTP on a mid with no owner yet (spec 10 section 7.1,
// mello-sfu 01-SFU.md section 5.4).
//
// RTP and the SFU's `slot` control message travel on separate transports, so
// the first packets of a new owner can arrive before the message (stage 4b
// soak: 0.009 % of slot packets, up to 116 ms early). VoiceSlotMap holds them
// for 200 ms; the bind plays them. Device-free: MELLO_AUDIO_BACKEND=test.

#include <gtest/gtest.h>

#include "context.hpp"
#include "mello.h"
#include "transport/peer_connection_impl.hpp"
#include "transport/voice_slot_map.hpp"

#include <cstdint>
#include <cstdlib>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#endif

namespace {

using mello::transport::PeerConnectionImpl;
using mello::transport::VoiceSlotMap;

constexpr int64_t kT0 = 1'000'000;

// One Opus RTP packet: the 3-byte Opus silence frame.
std::vector<uint8_t> opus_rtp(uint16_t seq, uint32_t ts, uint32_t ssrc) {
    return {
        0x80, 111,
        static_cast<uint8_t>(seq >> 8), static_cast<uint8_t>(seq),
        static_cast<uint8_t>(ts >> 24), static_cast<uint8_t>(ts >> 16),
        static_cast<uint8_t>(ts >> 8), static_cast<uint8_t>(ts),
        static_cast<uint8_t>(ssrc >> 24), static_cast<uint8_t>(ssrc >> 16),
        static_cast<uint8_t>(ssrc >> 8), static_cast<uint8_t>(ssrc),
        0xF8, 0xFF, 0xFE,
    };
}

uint16_t seq_of(const std::vector<uint8_t>& rtp) {
    return static_cast<uint16_t>((rtp[2] << 8) | rtp[3]);
}

std::string route(VoiceSlotMap& map, const char* mid, const std::vector<uint8_t>& rtp, int64_t at) {
    return map.route(mid, rtp.data(), rtp.size(), at);
}

TEST(VoiceSlotHoldTest, PacketsBeforeTheBindArePlayedInOrder) {
    VoiceSlotMap map;
    // A new owner's first packets, 100 to 60 ms before its slot message.
    EXPECT_EQ(route(map, "s0", opus_rtp(1, 960, 7), kT0), "");
    EXPECT_EQ(route(map, "s0", opus_rtp(2, 1920, 7), kT0 + 20), "");
    EXPECT_EQ(route(map, "s0", opus_rtp(3, 2880, 7), kT0 + 40), "");
    EXPECT_EQ(map.held(), 3u);

    const auto bound = map.bind("s0", "alice", kT0 + 100);
    ASSERT_EQ(bound.held.size(), 3u);
    EXPECT_EQ(seq_of(bound.held[0]), 1);
    EXPECT_EQ(seq_of(bound.held[1]), 2);
    EXPECT_EQ(seq_of(bound.held[2]), 3);
    EXPECT_EQ(map.held(), 0u);
    EXPECT_EQ(map.dropped(), 0u);
    EXPECT_EQ(route(map, "s0", opus_rtp(4, 3840, 7), kT0 + 120), "alice");
}

TEST(VoiceSlotHoldTest, APacketHeld300MsBeforeItsBindIsDroppedAndCounted) {
    VoiceSlotMap map;
    EXPECT_EQ(route(map, "s1", opus_rtp(1, 960, 9), kT0), "");
    EXPECT_EQ(route(map, "s1", opus_rtp(2, 1920, 9), kT0 + 250), "");

    // 300 ms after the first packet: only the second is young enough.
    const auto bound = map.bind("s1", "bob", kT0 + 300);
    ASSERT_EQ(bound.held.size(), 1u);
    EXPECT_EQ(seq_of(bound.held[0]), 2);
    EXPECT_EQ(map.dropped(), 1u);
}

TEST(VoiceSlotHoldTest, LatePacketsOfTheReleasedOwnerNeverReachTheNextOwner) {
    VoiceSlotMap map;
    map.bind("s2", "alice", kT0);
    EXPECT_EQ(route(map, "s2", opus_rtp(10, 100'000, 5), kT0 + 10), "alice");
    map.bind("s2", "", kT0 + 20);

    // A late packet of alice continues her timestamps: dropped at once.
    EXPECT_EQ(route(map, "s2", opus_rtp(11, 100'960, 5), kT0 + 30), "");
    EXPECT_EQ(map.dropped(), 1u);
    // The next owner's early packet has an unrelated timestamp: held.
    EXPECT_EQ(route(map, "s2", opus_rtp(12, 0x80000000u, 5), kT0 + 40), "");

    const auto bound = map.bind("s2", "carol", kT0 + 60);
    ASSERT_EQ(bound.previous, "");
    ASSERT_EQ(bound.held.size(), 1u);
    EXPECT_EQ(seq_of(bound.held[0]), 12);
}

TEST(VoiceSlotHoldTest, AFullHoldDropsTheOldestPacket) {
    VoiceSlotMap map;
    const size_t extra = 5;
    for (size_t i = 0; i < VoiceSlotMap::kMaxHeldPackets + extra; ++i) {
        route(map, "s3", opus_rtp(static_cast<uint16_t>(i), static_cast<uint32_t>(i * 480), 3),
              kT0 + static_cast<int64_t>(i));
    }
    EXPECT_EQ(map.held(), VoiceSlotMap::kMaxHeldPackets);
    EXPECT_EQ(map.dropped(), extra);
    const auto bound = map.bind("s3", "dave", kT0 + 50);
    ASSERT_EQ(bound.held.size(), VoiceSlotMap::kMaxHeldPackets);
    EXPECT_EQ(seq_of(bound.held.front()), extra);
}

TEST(VoiceSlotHoldTest, ClearCountsTheHeldPacketsAsDropped) {
    VoiceSlotMap map;
    route(map, "s4", opus_rtp(1, 960, 4), kT0);
    route(map, "s5", opus_rtp(1, 960, 6), kT0);
    map.clear();
    EXPECT_EQ(map.dropped(), 2u);
    EXPECT_EQ(map.held(), 0u);
}

void select_test_backend() {
#ifdef _WIN32
    SetEnvironmentVariableA("MELLO_AUDIO_BACKEND", "test");
#else
    setenv("MELLO_AUDIO_BACKEND", "test", 1);
#endif
}

// What the peer's RTP callback got, and the voice context it feeds, as
// mello-core does (mello_voice_feed_rtp from the track callback).
struct Feed {
    MelloContext* ctx = nullptr;
    std::vector<std::pair<std::string, uint16_t>> got;
};

void feed_voice(void* user_data, const char* sender, const uint8_t* data, int size) {
    auto* feed = static_cast<Feed*>(user_data);
    feed->got.emplace_back(sender, static_cast<uint16_t>((data[2] << 8) | data[3]));
    mello_voice_feed_rtp(feed->ctx, sender, data, size, -1);
}

MelloDebugStats stats(MelloContext* ctx) {
    MelloDebugStats s{};
    mello_get_debug_stats(ctx, &s);
    return s;
}

// Through a voice peer into the receive path: early packets reach the new
// owner's NetEQ in order once the slot message binds the mid; a packet that
// waited 300 ms does not, and MelloDebugStats counts it.
TEST(VoiceSlotHoldTest, EarlyPacketsReachTheOwnersNetEqAndStalePacketsAreCounted) {
    select_test_backend();
    MelloContext* ctx = mello_init();
    ASSERT_NE(ctx, nullptr);
    {
        auto peer = std::make_shared<PeerConnectionImpl>("sfu", MELLO_PEER_MEDIA_ROLE_VOICE);
        peer->set_voice_recv_slots(MELLO_VOICE_RECV_SLOTS);
        ASSERT_NE(peer->create_offer(), nullptr);
        // What mello_peer_create(ctx, ...) does.
        peer->set_unmapped_drop_counter(
            reinterpret_cast<mello::Context*>(ctx)->rx_unmapped_dropped_counter());
        Feed feed;
        feed.ctx = ctx;
        peer->set_audio_rtp_callback(feed_voice, &feed);

        for (uint16_t i = 1; i <= 3; ++i) {
            const auto pkt = opus_rtp(i, i * 960u, 0x5107);
            peer->route_voice_rtp("s0", pkt.data(), static_cast<int>(pkt.size()),
                                  kT0 + (i - 1) * 20);
        }
        EXPECT_TRUE(feed.got.empty()) << "no owner yet: nothing plays";

        peer->set_audio_mid_sender_at("s0", "alice", kT0 + 100);
        ASSERT_EQ(feed.got.size(), 3u);
        for (uint16_t i = 0; i < 3; ++i) {
            EXPECT_EQ(feed.got[i].first, "alice");
            EXPECT_EQ(feed.got[i].second, i + 1);
        }
        const auto next = opus_rtp(4, 4 * 960u, 0x5107);
        peer->route_voice_rtp("s0", next.data(), static_cast<int>(next.size()), kT0 + 120);
        ASSERT_EQ(feed.got.size(), 4u);
        EXPECT_EQ(feed.got[3].second, 4);

        auto s = stats(ctx);
        EXPECT_EQ(s.rx_packets_inserted, 4u) << "all four reached alice's NetEQ";
        EXPECT_EQ(s.incoming_streams, 1);
        EXPECT_EQ(s.rx_unmapped_dropped, 0u);

        // A packet 300 ms before its slot message.
        const auto stale = opus_rtp(1, 960, 0x6208);
        peer->route_voice_rtp("s1", stale.data(), static_cast<int>(stale.size()), kT0 + 200);
        peer->set_audio_mid_sender_at("s1", "bob", kT0 + 500);
        EXPECT_EQ(feed.got.size(), 4u) << "the stale packet must not play";
        s = stats(ctx);
        EXPECT_EQ(s.rx_unmapped_dropped, 1u);
        EXPECT_EQ(s.rx_packets_inserted, 4u);

        peer->set_audio_rtp_callback(nullptr, nullptr);
    }
    mello_destroy(ctx);
}

}  // namespace
