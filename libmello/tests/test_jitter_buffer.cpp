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
    uint32_t seq = 0;
    // Drain the original half-buffer in sequence order, topping the buffer
    // back up to kHalfFull after each pop so the hold gate stays bypassed.
    for (uint32_t i = 0; i < kHalfFull; ++i) {
        ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet)
            << "seq " << i;
        EXPECT_EQ(seq, i);
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
    uint32_t seq = 0;
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 2u);
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
    uint32_t seq = 0;
    // Out-of-order arrivals must be released in sequence order: 0, 1, 2.
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 0u);
    EXPECT_EQ(out, make_data(0xD0));

    push(kHalfFull, 0xDD);  // top up to keep the hold gate bypassed
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 1u);
    EXPECT_EQ(out, make_data(0xD1));

    push(kHalfFull + 1, 0xDD);
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 2u);
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
    uint32_t seq = 0;
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 0u);

    // Expected seq 1 is absent and >= JITTER_MAX_PACKETS/3 newer packets are
    // buffered: pop reports Missing (concealment signal) and skips next_seq_.
    EXPECT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Missing);
    EXPECT_EQ(jb.underruns(), 1u);

    // Playout resumes at the oldest buffered packet: skip-ahead past the gap.
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 2u);
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
    uint32_t seq = 0;
    ASSERT_EQ(jb.pop(out, &seq), mello::audio::JitterPopResult::Packet);
    EXPECT_EQ(seq, 0u);
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
