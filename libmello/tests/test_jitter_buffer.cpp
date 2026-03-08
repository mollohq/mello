#include <gtest/gtest.h>
#include "audio/jitter_buffer.hpp"
#include <vector>

using namespace mello::audio;

class JitterBufferTest : public ::testing::Test {
protected:
    JitterBuffer jb;

    std::vector<uint8_t> make_data(uint8_t tag, int size = 10) {
        return std::vector<uint8_t>(size, tag);
    }
};

TEST_F(JitterBufferTest, PushPopInOrder) {
    jb.push(0, make_data(0xA0).data(), 10);
    jb.push(1, make_data(0xA1).data(), 10);
    jb.push(2, make_data(0xA2).data(), 10);

    std::vector<uint8_t> out;
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xA0));
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xA1));
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xA2));
    EXPECT_FALSE(jb.pop(out)) << "should be empty";
}

TEST_F(JitterBufferTest, OutOfOrderReorder) {
    jb.push(2, make_data(0xC2).data(), 10);
    jb.push(0, make_data(0xC0).data(), 10);
    jb.push(1, make_data(0xC1).data(), 10);

    std::vector<uint8_t> out;

    // First push sets next_seq_ to 2 (first packet seen), so seq 0 is "old" and dropped.
    // The buffer should yield 2 first, then nothing for 3.
    // Actually: first_packet_ sets next_seq_ = sequence of first push = 2.
    // seq 0 < next_seq_(2), so it's dropped. seq 1 < next_seq_(2), also dropped.
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xC2));
    EXPECT_FALSE(jb.pop(out));
}

TEST_F(JitterBufferTest, OutOfOrderCloseSequences) {
    // Push in order so first_packet sees seq 0
    jb.push(0, make_data(0xD0).data(), 10);
    jb.push(2, make_data(0xD2).data(), 10);
    jb.push(1, make_data(0xD1).data(), 10);

    std::vector<uint8_t> out;
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xD0));
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xD1));
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xD2));
}

TEST_F(JitterBufferTest, PacketLossSkipAhead) {
    jb.push(0, make_data(0xE0).data(), 10);

    std::vector<uint8_t> out;
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xE0));

    // next_seq_ is now 1. Push 5,6,7 (gap > 3 from next_seq_=1)
    jb.push(5, make_data(0xE5).data(), 10);
    jb.push(6, make_data(0xE6).data(), 10);
    jb.push(7, make_data(0xE7).data(), 10);

    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xE5)) << "should skip ahead to seq 5";
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xE6));
    ASSERT_TRUE(jb.pop(out));
    EXPECT_EQ(out, make_data(0xE7));
}

TEST_F(JitterBufferTest, DuplicateRejection) {
    jb.push(0, make_data(0xF0).data(), 10);
    jb.push(0, make_data(0xFF).data(), 10);  // duplicate seq

    std::vector<uint8_t> out;
    ASSERT_TRUE(jb.pop(out));
    // Map overwrites, so second push wins
    EXPECT_FALSE(jb.pop(out)) << "only one packet should be available for seq 0";
}

TEST_F(JitterBufferTest, MaxCapacity) {
    for (uint32_t i = 0; i < JITTER_MAX_PACKETS + 10; ++i) {
        uint8_t tag = static_cast<uint8_t>(i & 0xFF);
        jb.push(i, make_data(tag).data(), 10);
    }
    EXPECT_LE(jb.buffered_count(), JITTER_MAX_PACKETS);
}

TEST_F(JitterBufferTest, Reset) {
    jb.push(0, make_data(0x01).data(), 10);
    jb.push(1, make_data(0x02).data(), 10);
    EXPECT_GT(jb.buffered_count(), 0);

    jb.reset();
    EXPECT_EQ(jb.buffered_count(), 0);

    std::vector<uint8_t> out;
    EXPECT_FALSE(jb.pop(out));
}
