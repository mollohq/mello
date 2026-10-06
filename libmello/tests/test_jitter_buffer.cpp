#include <gtest/gtest.h>
#include "audio/jitter_buffer.hpp"
#include <vector>

using namespace mello::audio;

// The neteq-style jitter buffer gates pop() on wall-clock hold time
// (target_delay_ms_, initially JITTER_TARGET_MS = 60). These tests avoid
// sleeps by using the deterministic buffer-level overrides instead:
//   - buffer >= JITTER_MAX_PACKETS/2 (25) bypasses the hold-time gate, and
//     also satisfies the prebuffering requirement (max(2, target/20) packets);
//   - buffer >= JITTER_MAX_PACKETS/3 (16) triggers Missing for a lost
//     expected sequence number.
// Both overrides are independent of the adapted target delay, so the tests
// do not depend on timing.

class JitterBufferTest : public ::testing::Test {
protected:
    static constexpr uint32_t kHalfFull = JITTER_MAX_PACKETS / 2;  // 25

    JitterBuffer jb;

    std::vector<uint8_t> make_data(uint8_t tag, int size = 10) {
        return std::vector<uint8_t>(size, tag);
    }

    void push(uint32_t seq, uint8_t tag) {
        auto d = make_data(tag);
        jb.push(seq, d.data(), static_cast<int>(d.size()));
    }
};

TEST_F(JitterBufferTest, PushPopInOrder) {
    // Fill to half capacity so pops are gated by buffer level, not hold time.
    for (uint32_t i = 0; i < kHalfFull; ++i) {
        push(i, static_cast<uint8_t>(i));
    }

    std::vector<uint8_t> out;
    int64_t seq = 0;
    // Drain the original half-buffer in sequence order, topping the buffer
    // back up to kHalfFull after each pop so the hold gate stays bypassed.
    for (uint32_t i = 0; i < kHalfFull; ++i) {
        ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet)
            << "seq " << i;
        EXPECT_EQ(seq, static_cast<int64_t>(i));
        EXPECT_EQ(out, make_data(static_cast<uint8_t>(i)));
        if (i + 1 < kHalfFull) {
            push(kHalfFull + i, 0xEE);  // keep buffer at kHalfFull
        }
    }

    // 24 fresh packets remain (< kHalfFull, held ~0ms < target_delay_ms_):
    // the playout-delay gate blocks further pops until they age.
    EXPECT_EQ(jb.pop(out), mello::audio::JitterPopResult::None);
}

TEST_F(JitterBufferTest, OutOfOrderReorder) {
    // The first packet seen anchors the playout timeline: next_seq_ = 2.
    push(2, 0xC2);
    // Packets older than next_seq_ arriving on a non-empty buffer are dropped.
    push(0, 0xC0);
    push(1, 0xC1);
    EXPECT_EQ(jb.buffered_count(), 1)
        << "stale packets (seq < next_seq_) must be dropped";

    // Fill to half capacity (newer sequences) to bypass prebuffer/hold gates.
    for (uint32_t s = 3; s < 3 + (kHalfFull - 1); ++s) {
        push(s, 0xEE);
    }

    std::vector<uint8_t> out;
    int64_t seq = 0;
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 2);
    EXPECT_EQ(out, make_data(0xC2));

    // Remaining packets are fresh and buffer < half full: hold gate blocks.
    EXPECT_EQ(jb.pop(out), mello::audio::JitterPopResult::None);
}

TEST_F(JitterBufferTest, OutOfOrderCloseSequences) {
    // In-order first packet anchors next_seq_ = 0; 2 and 1 arrive out of order.
    push(0, 0xD0);
    push(2, 0xD2);
    push(1, 0xD1);
    // Fill to half capacity: buffer now holds seqs 0..24.
    for (uint32_t s = 3; s < kHalfFull; ++s) {
        push(s, 0xDD);
    }

    std::vector<uint8_t> out;
    int64_t seq = 0;
    // Out-of-order arrivals must be released in sequence order: 0, 1, 2.
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 0);
    EXPECT_EQ(out, make_data(0xD0));

    push(kHalfFull, 0xDD);  // top up to keep the hold gate bypassed
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 1);
    EXPECT_EQ(out, make_data(0xD1));

    push(kHalfFull + 1, 0xDD);
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 2);
    EXPECT_EQ(out, make_data(0xD2));

    // Buffer dropped below half full with fresh packets: hold gate re-engages.
    EXPECT_EQ(jb.pop(out), mello::audio::JitterPopResult::None);
}

TEST_F(JitterBufferTest, PacketLossSkipAhead) {
    // seq 1 never arrives (lost). Buffer holds {0, 2..26}: 26 packets.
    push(0, 0xE0);
    for (uint32_t s = 2; s <= kHalfFull + 1; ++s) {
        push(s, static_cast<uint8_t>(s));
    }

    std::vector<uint8_t> out;
    int64_t seq = 0;
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 0);

    // Expected seq 1 is absent and >= JITTER_MAX_PACKETS/3 newer packets are
    // buffered: pop reports Missing (concealment signal) and skips next_seq_.
    EXPECT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Missing);
    EXPECT_EQ(jb.underruns(), 1u);

    // Playout resumes at the oldest buffered packet: skip-ahead past the gap.
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 2);
    EXPECT_EQ(out, make_data(2));
}

TEST_F(JitterBufferTest, DuplicateRejection) {
    push(0, 0xF0);
    push(0, 0xFF);  // same sequence: overwrites in place, never double-buffers
    EXPECT_EQ(jb.buffered_count(), 1);

    // Fill to half capacity to bypass the hold gate.
    for (uint32_t s = 1; s < kHalfFull; ++s) {
        push(s, 0xEE);
    }

    std::vector<uint8_t> out;
    int64_t seq = 0;
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 0);
    EXPECT_EQ(out, make_data(0xFF)) << "latest write wins for a duplicate sequence";

    // Exactly one packet existed for seq 0; the rest are fresh and the buffer
    // is below half full, so the hold gate blocks the next pop.
    EXPECT_EQ(jb.pop(out), mello::audio::JitterPopResult::None);
}

TEST_F(JitterBufferTest, MaxCapacity) {
    // Pushing past capacity evicts the oldest (lowest sequence) packet, so
    // the buffer pins at exactly JITTER_MAX_PACKETS.
    for (uint32_t i = 0; i < JITTER_MAX_PACKETS + 10; ++i) {
        push(i, static_cast<uint8_t>(i & 0xFF));
    }
    EXPECT_EQ(jb.buffered_count(), JITTER_MAX_PACKETS);
}

TEST_F(JitterBufferTest, Reset) {
    push(0, 0x01);
    push(1, 0x02);
    EXPECT_GT(jb.buffered_count(), 0);

    jb.reset();
    EXPECT_EQ(jb.buffered_count(), 0);

    // Empty buffer pops None (prebuffering state was also reset).
    std::vector<uint8_t> out;
    EXPECT_EQ(jb.pop(out), mello::audio::JitterPopResult::None);
}

// ---------------------------------------------------------------------------
// Timeline tests on the playout clock. util::set_test_clock_ms() drives the
// hold timing, so a packet is releasable once the clock moves past its
// target delay. No sleeps.
// ---------------------------------------------------------------------------

#include "util/test_clock.hpp"

class JitterTimelineTest : public ::testing::Test {
protected:
    static constexpr int64_t kStart = 1'000'000;

    JitterBuffer jb;
    int64_t now = kStart;

    void SetUp() override { mello::util::set_test_clock_ms(now); }
    void TearDown() override { mello::util::set_test_clock_ms(-1); }

    void advance(int64_t ms) {
        now += ms;
        mello::util::set_test_clock_ms(now);
    }

    void push(uint32_t seq, uint8_t tag) {
        std::vector<uint8_t> d(10, tag);
        jb.push(seq, d.data(), static_cast<int>(d.size()));
    }

    // Pops until None. Returns one entry per pop: the payload tag for a
    // Packet, -1 for a Missing.
    std::vector<int> drain() {
        std::vector<int> got;
        std::vector<uint8_t> out;
        for (;;) {
            const auto r = jb.pop(out);
            if (r == JitterPopResult::None) break;
            got.push_back(r == JitterPopResult::Packet ? static_cast<int>(out.at(0)) : -1);
        }
        return got;
    }
};

// The SFU path carries the 16-bit RTP sequence in the packet header
// (peer_connection.cpp). The buffer must play straight through the wrap from
// 65535 to 0: no late drop, no reset, no Missing.
TEST_F(JitterTimelineTest, SixteenBitSequenceWrapPlaysThrough) {
    std::vector<int> expected;
    for (uint32_t i = 0; i < 30; ++i) {
        push((65520u + i) & 0xFFFFu, static_cast<uint8_t>(i));
        expected.push_back(static_cast<int>(i));
        advance(20);
    }
    advance(1000);

    EXPECT_EQ(drain(), expected);
    EXPECT_EQ(jb.dropped_late(), 0u);
    EXPECT_EQ(jb.discontinuity_resets(), 0u);
    EXPECT_EQ(jb.underruns(), 0u);
}

// A packet lost exactly at the wrap is one Missing, then playout continues.
TEST_F(JitterTimelineTest, LossAtTheWrapIsOneMissing) {
    push(65534, 1);
    advance(20);
    // 65535 is lost.
    push(0, 3);
    advance(20);
    push(1, 4);
    advance(1000);

    EXPECT_EQ(drain(), (std::vector<int>{1, -1, 3, 4}));
    EXPECT_EQ(jb.dropped_late(), 0u);
    EXPECT_EQ(jb.discontinuity_resets(), 0u);
}

// Reorder across the wrap: 0 arrives before 65535. Both play, in order.
TEST_F(JitterTimelineTest, ReorderAcrossTheWrap) {
    push(65534, 1);
    advance(20);
    push(0, 3);
    push(65535, 2);
    advance(20);
    push(1, 4);
    advance(1000);

    EXPECT_EQ(drain(), (std::vector<int>{1, 2, 3, 4}));
    EXPECT_EQ(jb.dropped_late(), 0u);
}

// The P2P path sends a true 32-bit sequence (AudioPipeline::get_packet). It
// crosses 65536 without a wrap and must keep working.
TEST_F(JitterTimelineTest, ThirtyTwoBitSequencePlaysPast65536) {
    std::vector<int> expected;
    for (uint32_t i = 0; i < 20; ++i) {
        push(65530u + i, static_cast<uint8_t>(i));
        expected.push_back(static_cast<int>(i));
        advance(20);
    }
    advance(1000);

    EXPECT_EQ(drain(), expected);
    EXPECT_EQ(jb.dropped_late(), 0u);
    EXPECT_EQ(jb.discontinuity_resets(), 0u);
}

// A track re-wire starts a new sequence space. On an empty buffer the jump is
// a discontinuity: the buffer resets and plays the new stream.
TEST_F(JitterTimelineTest, LargeJumpOnAnEmptyBufferResets) {
    push(100, 1);
    advance(1000);
    EXPECT_EQ(drain(), (std::vector<int>{1}));

    push(30000, 2);
    advance(1000);
    EXPECT_EQ(drain(), (std::vector<int>{2}));
    EXPECT_EQ(jb.discontinuity_resets(), 1u);
}

TEST(SequenceUnwrapperTest, ExtendsAcrossTheWrapBothWays) {
    SequenceUnwrapper u;
    EXPECT_EQ(u.unwrap(65534), 65534);
    EXPECT_EQ(u.unwrap(65535), 65535);
    EXPECT_EQ(u.unwrap(0), 65536);
    EXPECT_EQ(u.unwrap(1), 65537);
    // A reordered packet from before the wrap keeps its place.
    EXPECT_EQ(u.unwrap(65535), 65535);
    EXPECT_EQ(u.unwrap(2), 65538);
    // A 32-bit P2P value unwraps to the same timeline as its low 16 bits.
    SequenceUnwrapper p2p;
    EXPECT_EQ(p2p.unwrap(70000), 70000 - 65536);
    EXPECT_EQ(p2p.unwrap(70001), 70001 - 65536);
}
