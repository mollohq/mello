// Tests for the hook protocol and the hook's view of it.
//
// These need no game and no GPU. They cover the two things that break silently
// in production: a layout change between the two binaries, and a hook that
// keeps capturing after the client has gone.

#include <gtest/gtest.h>

#include <windows.h>

#include <cstddef>

#include "hook_state.hpp"
#include "mello_hook_protocol.h"

using namespace mello_hook;

namespace {

// Stands in for the client: creates the block and the events the way libmello
// does, for this process id, and cleans them up afterwards.
class ClientSide {
public:
    bool create() {
        const uint32_t pid = GetCurrentProcessId();
        char name[64];

        object_name(name, sizeof(name), MELLO_HOOK_NAME_INFO, pid);
        mapping_ = CreateFileMappingA(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE, 0,
                                      sizeof(MelloHookInfo), name);
        if (!mapping_) return false;
        info_ = static_cast<MelloHookInfo*>(
            MapViewOfFile(mapping_, FILE_MAP_ALL_ACCESS, 0, 0, sizeof(MelloHookInfo)));
        if (!info_) return false;
        ZeroMemory(info_, sizeof(MelloHookInfo));
        info_->protocol_version = MELLO_HOOK_PROTOCOL_VERSION;
        info_->struct_size = sizeof(MelloHookInfo);
        info_->client_pid = pid;

        object_name(name, sizeof(name), MELLO_HOOK_NAME_READY, pid);
        ready_ = CreateEventA(nullptr, TRUE, FALSE, name);
        object_name(name, sizeof(name), MELLO_HOOK_NAME_FRAME, pid);
        frame_ = CreateEventA(nullptr, FALSE, FALSE, name);
        object_name(name, sizeof(name), MELLO_HOOK_NAME_STOP, pid);
        stop_ = CreateEventA(nullptr, TRUE, FALSE, name);
        return ready_ && frame_ && stop_;
    }

    ~ClientSide() {
        if (info_) UnmapViewOfFile(info_);
        for (HANDLE h : {mapping_, ready_, frame_, stop_}) {
            if (h) CloseHandle(h);
        }
    }

    MelloHookInfo* info() { return info_; }
    HANDLE ready() { return ready_; }
    HANDLE frame() { return frame_; }
    HANDLE stop() { return stop_; }

    void beat() {
        LARGE_INTEGER now{};
        QueryPerformanceCounter(&now);
        info_->heartbeat_qpc = static_cast<uint64_t>(now.QuadPart);
    }

private:
    HANDLE         mapping_ = nullptr;
    MelloHookInfo* info_    = nullptr;
    HANDLE         ready_   = nullptr;
    HANDLE         frame_   = nullptr;
    HANDLE         stop_    = nullptr;
};

}  // namespace

// The hook and the client ship as separate files and can be different versions
// after a failed update. Every field offset is part of the contract.
TEST(HookProtocol, LayoutIsFixed) {
    EXPECT_EQ(sizeof(MelloHookInfo), 248u);
    EXPECT_EQ(offsetof(MelloHookInfo, protocol_version), 0u);
    EXPECT_EQ(offsetof(MelloHookInfo, struct_size), 4u);
    EXPECT_EQ(offsetof(MelloHookInfo, adapter_luid), 32u);
    EXPECT_EQ(offsetof(MelloHookInfo, shared_handles), 40u);
    EXPECT_EQ(offsetof(MelloHookInfo, capture_enabled), 64u);
    EXPECT_EQ(offsetof(MelloHookInfo, frame_index), 72u);
    EXPECT_EQ(offsetof(MelloHookInfo, heartbeat_qpc), 88u);
    EXPECT_EQ(offsetof(MelloHookInfo, offsets_valid), 112u);
    EXPECT_EQ(offsetof(MelloHookInfo, off_dxgi_present), 120u);
}

TEST(HookProtocol, NamesCarryTheProcessId) {
    char name[64];
    object_name(name, sizeof(name), MELLO_HOOK_NAME_INFO, 4321);
    EXPECT_STREQ(name, "Local\\mello_hook_info_4321");
}

// A hook loaded into a process m3llo never asked for must do nothing at all.
TEST(HookState, DormantWithoutAClientBlock) {
    HookState& state = HookState::instance();
    state.close();
    EXPECT_FALSE(state.open());
    EXPECT_FALSE(state.is_open());
    EXPECT_FALSE(state.capture_wanted());
}

TEST(HookState, OpensTheBlockTheClientCreated) {
    ClientSide client;
    ASSERT_TRUE(client.create());

    HookState& state = HookState::instance();
    state.close();
    ASSERT_TRUE(state.open());
    EXPECT_EQ(state.info()->hook_pid, GetCurrentProcessId());
    EXPECT_EQ(state.info()->hook_bitness, static_cast<uint32_t>(MELLO_HOOK_BITS));
    state.close();
}

// The client asks for frames with `capture_enabled`, and proves it is still
// there with the heartbeat. Both are needed: a client that died leaves
// `capture_enabled` set in memory the hook can still read.
TEST(HookState, CaptureNeedsBothTheRequestAndTheHeartbeat) {
    ClientSide client;
    ASSERT_TRUE(client.create());

    HookState& state = HookState::instance();
    state.close();
    ASSERT_TRUE(state.open());

    EXPECT_FALSE(state.capture_wanted()) << "no request and no heartbeat";

    client.info()->capture_enabled = 1;
    EXPECT_FALSE(state.capture_wanted()) << "asked for frames but never proved it is alive";

    client.beat();
    EXPECT_TRUE(state.capture_wanted());

    client.info()->capture_enabled = 0;
    EXPECT_FALSE(state.capture_wanted()) << "the client stopped the stream";

    state.close();
}

TEST(HookState, AStaleHeartbeatStopsCapture) {
    ClientSide client;
    ASSERT_TRUE(client.create());

    HookState& state = HookState::instance();
    state.close();
    ASSERT_TRUE(state.open());

    LARGE_INTEGER freq{}, now{};
    QueryPerformanceFrequency(&freq);
    QueryPerformanceCounter(&now);

    client.info()->capture_enabled = 1;
    // One millisecond past the timeout: the client is gone as far as the hook
    // is concerned, and the game must stop paying for capture.
    const int64_t stale_ms = MELLO_HOOK_HEARTBEAT_TIMEOUT_MS + 1;
    client.info()->heartbeat_qpc =
        static_cast<uint64_t>(now.QuadPart - (stale_ms * freq.QuadPart / 1000));
    EXPECT_FALSE(state.capture_wanted());

    client.beat();
    EXPECT_TRUE(state.capture_wanted());
    state.close();
}

// The frame counter is what the client waits on. It must move with the texture
// index it belongs to, and the frame event must wake a waiting client.
TEST(HookState, PublishingAFrameMovesTheCounterAndSignals) {
    ClientSide client;
    ASSERT_TRUE(client.create());

    HookState& state = HookState::instance();
    state.close();
    ASSERT_TRUE(state.open());

    EXPECT_EQ(client.info()->frame_index, 0u);
    state.publish_frame(1, 12345);
    state.signal_frame();

    EXPECT_EQ(client.info()->frame_index, 1u);
    EXPECT_EQ(client.info()->texture_index, 1u);
    EXPECT_EQ(client.info()->frame_qpc, 12345u);
    EXPECT_EQ(WaitForSingleObject(client.frame(), 0), WAIT_OBJECT_0);

    state.publish_frame(0, 12400);
    EXPECT_EQ(client.info()->frame_index, 2u);
    EXPECT_EQ(client.info()->texture_index, 0u);
    state.close();
}

// The description changes when a game resizes. The generation is what tells the
// client to open the new textures, so it must change after the fields do.
TEST(HookState, DescriptionBumpsTheGeneration) {
    ClientSide client;
    ASSERT_TRUE(client.create());

    HookState& state = HookState::instance();
    state.close();
    ASSERT_TRUE(state.open());

    const uint32_t handles[MELLO_HOOK_TEXTURE_COUNT] = {0xAAAA, 0xBBBB};
    state.publish_description(MELLO_HOOK_API_D3D11, 1920, 1080, 87, 0x1234, handles,
                              MELLO_HOOK_FLAG_SRGB);

    EXPECT_EQ(client.info()->texture_generation, 1u);
    EXPECT_EQ(client.info()->width, 1920u);
    EXPECT_EQ(client.info()->height, 1080u);
    EXPECT_EQ(client.info()->dxgi_format, 87u);
    EXPECT_EQ(client.info()->adapter_luid, 0x1234u);
    EXPECT_EQ(client.info()->shared_handles[0], 0xAAAAu);
    EXPECT_EQ(client.info()->shared_handles[1], 0xBBBBu);
    EXPECT_EQ(client.info()->flags, MELLO_HOOK_FLAG_SRGB);

    const uint32_t again[MELLO_HOOK_TEXTURE_COUNT] = {1, 2};
    state.publish_description(MELLO_HOOK_API_D3D11, 800, 600, 28, 0x1234, again, 0);
    EXPECT_EQ(client.info()->texture_generation, 2u);
    state.close();
}

TEST(HookState, StopEventEndsTheWait) {
    ClientSide client;
    ASSERT_TRUE(client.create());

    HookState& state = HookState::instance();
    state.close();
    ASSERT_TRUE(state.open());

    EXPECT_FALSE(state.wait_for_stop(0));
    SetEvent(client.stop());
    EXPECT_TRUE(state.wait_for_stop(0));
    state.close();
}
