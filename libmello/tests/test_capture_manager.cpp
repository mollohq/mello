// CaptureManager: the frame and audio fan-out to the consumers, and the
// add and remove rules (14-VIDEO-PIPELINE.md 4.8). A fake source
// delivers on the test's own threads, so no GPU and no timing is involved.
#include <gtest/gtest.h>
#include "video/capture_manager.hpp"

#include <atomic>
#include <chrono>
#include <future>
#include <memory>
#include <mutex>
#include <thread>
#include <utility>
#include <vector>

using namespace mello::video;

namespace {

// What the fake source did, kept apart from it: the manager owns and frees
// the source, and the test still reads this afterwards.
struct FakeState {
    bool initialize_ok = true;
    bool start_ok = true;
    bool stop_times_out = false;

    std::atomic<int> starts{0};
    std::atomic<int> stops{0};
    std::atomic<uint32_t> start_fps{0};
    std::atomic<bool> swap_pending{false};
    std::atomic<int> swaps_consumed{0};
    std::atomic<bool> destroyed{false};
    PresentDelayHistogram* histogram = nullptr;

    std::mutex frame_mutex;
    CaptureSource::FrameCallback frame_cb;

    // The source calls its audio callbacks under this lock, as the reference
    // source does, so a setter waits for a call that runs.
    std::mutex audio_mutex;
    CaptureSource::AudioCallback audio_cb;
    CaptureSource::TimedAudioCallback timed_cb;
};

class FakeSource : public CaptureSource {
public:
    explicit FakeSource(std::shared_ptr<FakeState> state) : s_(std::move(state)) {}
    ~FakeSource() override { s_->destroyed = true; }

    bool initialize(const GraphicsDevice&, const CaptureSourceDesc&) override {
        return s_->initialize_ok;
    }
    bool start(uint32_t target_fps, FrameCallback callback) override {
        s_->starts.fetch_add(1);
        s_->start_fps = target_fps;
        if (!s_->start_ok) return false;
        std::lock_guard<std::mutex> lock(s_->frame_mutex);
        s_->frame_cb = std::move(callback);
        return true;
    }
    void stop() override {
        s_->stops.fetch_add(1);
        std::lock_guard<std::mutex> lock(s_->frame_mutex);
        s_->frame_cb = nullptr;
    }
    uint32_t width() const override { return 1280; }
    uint32_t height() const override { return 720; }
    const char* backend_name() const override { return "Fake"; }
    bool consume_swap_event() override {
        const bool swapped = s_->swap_pending.exchange(false);
        if (swapped) s_->swaps_consumed.fetch_add(1);
        return swapped;
    }
    bool provides_timed_audio() const override { return true; }
    void set_audio_callback(AudioCallback cb) override {
        std::lock_guard<std::mutex> lock(s_->audio_mutex);
        s_->audio_cb = std::move(cb);
    }
    void set_timed_audio_callback(TimedAudioCallback cb) override {
        std::lock_guard<std::mutex> lock(s_->audio_mutex);
        s_->timed_cb = std::move(cb);
    }
    bool stop_timed_out() const override { return s_->stop_times_out && s_->stops.load() > 0; }
    void set_present_delay_histogram(PresentDelayHistogram* hist) override {
        s_->histogram = hist;
    }

private:
    std::shared_ptr<FakeState> s_;
};

// One frame from the source's capture thread (the caller's thread here).
void push_frame(FakeState& s, uintptr_t id, uint64_t capture_us) {
    CaptureSource::FrameCallback cb;
    {
        std::lock_guard<std::mutex> lock(s.frame_mutex);
        cb = s.frame_cb;
    }
    if (cb) cb(reinterpret_cast<ID3D11Texture2D*>(id), capture_us);
}

const float kSamples[4] = {0.1f, -0.1f, 0.2f, -0.2f};

void push_audio(FakeState& s) {
    std::lock_guard<std::mutex> lock(s.audio_mutex);
    if (s.audio_cb) s.audio_cb(kSamples, 2, 2, 48'000);
}

void push_timed_audio(FakeState& s, int64_t capture_us) {
    std::lock_guard<std::mutex> lock(s.audio_mutex);
    if (s.timed_cb) s.timed_cb(kSamples, 2, 2, 48'000, capture_us);
}

bool source_has_audio_callback(FakeState& s) {
    std::lock_guard<std::mutex> lock(s.audio_mutex);
    return static_cast<bool>(s.audio_cb);
}

bool source_has_timed_callback(FakeState& s) {
    std::lock_guard<std::mutex> lock(s.audio_mutex);
    return static_cast<bool>(s.timed_cb);
}

CaptureSourceDesc monitor_desc() {
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Monitor;
    desc.monitor_index = 0;
    return desc;
}

std::shared_ptr<CaptureManager> make_manager(const std::shared_ptr<FakeState>& state) {
    return CaptureManager::create_from(GraphicsDevice{}, std::make_unique<FakeSource>(state),
                                       monitor_desc());
}

// The frames one consumer saw, in order.
struct FrameLog {
    struct Entry {
        char consumer;
        uintptr_t texture;
        uint64_t capture_us;
        uint32_t swap_generation;
    };
    std::mutex mutex;
    std::vector<Entry> entries;

    CaptureManager::FrameCallback callback(char consumer) {
        return [this, consumer](const CapturedFrame& f) {
            std::lock_guard<std::mutex> lock(mutex);
            entries.push_back({consumer, reinterpret_cast<uintptr_t>(f.texture), f.capture_us,
                               f.swap_generation});
        };
    }
    std::vector<uintptr_t> textures_of(char consumer) {
        std::lock_guard<std::mutex> lock(mutex);
        std::vector<uintptr_t> out;
        for (const auto& e : entries) {
            if (e.consumer == consumer) out.push_back(e.texture);
        }
        return out;
    }
};

}  // namespace

TEST(CaptureManager, AFailedInitializeGivesNoManagerAndFreesTheSource) {
    auto state = std::make_shared<FakeState>();
    state->initialize_ok = false;
    EXPECT_EQ(make_manager(state), nullptr);
    EXPECT_TRUE(state->destroyed);
}

TEST(CaptureManager, TheSourceGetsTheHistogramAndTheQueriesReachTheSource) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);
    ASSERT_NE(state->histogram, nullptr);

    state->histogram->record_ms(3.2);
    uint32_t buckets[PresentDelayHistogram::kBuckets] = {};
    manager->present_delay_snapshot(buckets);
    EXPECT_EQ(buckets[3], 1u);

    EXPECT_EQ(manager->width(), 1280u);
    EXPECT_EQ(manager->height(), 720u);
    EXPECT_STREQ(manager->backend_name(), "Fake");
    EXPECT_TRUE(manager->provides_timed_audio());
    EXPECT_FALSE(manager->failed());
    EXPECT_EQ(manager->state(), CaptureState::Capturing);
}

TEST(CaptureManager, EveryConsumerGetsEveryFrameInOrder) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);
    FrameLog log;
    const auto a = manager->add_consumer(log.callback('a'));
    const auto b = manager->add_consumer(log.callback('b'));
    ASSERT_NE(a, 0u);
    ASSERT_NE(b, 0u);
    ASSERT_TRUE(manager->start(60));
    EXPECT_EQ(state->starts.load(), 1);
    EXPECT_EQ(state->start_fps.load(), 60u);

    for (uintptr_t i = 1; i <= 5; ++i) push_frame(*state, i, 1000 * i);

    ASSERT_EQ(log.entries.size(), 10u);
    for (size_t i = 0; i < 5; ++i) {
        const auto& first = log.entries[2 * i];
        const auto& second = log.entries[2 * i + 1];
        EXPECT_EQ(first.consumer, 'a');
        EXPECT_EQ(second.consumer, 'b');
        EXPECT_EQ(first.texture, i + 1);
        EXPECT_EQ(second.texture, i + 1);
        EXPECT_EQ(first.capture_us, 1000 * (i + 1));
        EXPECT_EQ(second.capture_us, 1000 * (i + 1));
    }
    EXPECT_EQ(manager->remove_consumer(b), CaptureManager::RemoveResult::Removed);
    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Removed);
}

TEST(CaptureManager, ARemovedConsumerGetsNoMoreFramesAndTheOtherKeepsThem) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);
    FrameLog log;
    const auto a = manager->add_consumer(log.callback('a'));
    const auto b = manager->add_consumer(log.callback('b'));
    ASSERT_TRUE(manager->start(60));

    push_frame(*state, 1, 1);
    push_frame(*state, 2, 2);
    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Removed);
    // Another consumer is attached: the source keeps running.
    EXPECT_EQ(state->stops.load(), 0);
    push_frame(*state, 3, 3);
    push_frame(*state, 4, 4);

    EXPECT_EQ(log.textures_of('a'), (std::vector<uintptr_t>{1, 2}));
    EXPECT_EQ(log.textures_of('b'), (std::vector<uintptr_t>{1, 2, 3, 4}));
    EXPECT_EQ(manager->remove_consumer(b), CaptureManager::RemoveResult::Removed);
}

TEST(CaptureManager, RemoveWaitsForACallInFlight) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);

    std::promise<void> entered;
    std::promise<void> release;
    std::shared_future<void> released = release.get_future().share();
    std::atomic<int> calls{0};
    std::atomic<bool> returned{false};
    const auto a = manager->add_consumer([&](const CapturedFrame&) {
        if (calls.fetch_add(1) == 0) {
            entered.set_value();
            released.wait();
            returned = true;
        }
    });
    FrameLog log;
    const auto b = manager->add_consumer(log.callback('b'));
    ASSERT_TRUE(manager->start(60));

    std::thread capture([&] { push_frame(*state, 1, 1); });
    entered.get_future().wait();

    auto removing = std::async(std::launch::async, [&] { return manager->remove_consumer(a); });
    // The call to `a` still runs, so the remove cannot have returned.
    EXPECT_EQ(removing.wait_for(std::chrono::milliseconds(100)), std::future_status::timeout);
    release.set_value();
    EXPECT_EQ(removing.get(), CaptureManager::RemoveResult::Removed);
    EXPECT_TRUE(returned);
    capture.join();

    push_frame(*state, 2, 2);
    EXPECT_EQ(calls.load(), 1);
    EXPECT_EQ(log.textures_of('b'), (std::vector<uintptr_t>{1, 2}));
    EXPECT_EQ(manager->remove_consumer(b), CaptureManager::RemoveResult::Removed);
}

TEST(CaptureManager, RemoveGivesUpOnAStuckConsumerAfterTheDeadline) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);

    std::promise<void> entered;
    std::promise<void> release;
    std::shared_future<void> released = release.get_future().share();
    std::atomic<int> calls{0};
    const auto a = manager->add_consumer([&](const CapturedFrame&) {
        if (calls.fetch_add(1) == 0) {
            entered.set_value();
            released.wait();
        }
    });
    FrameLog log;
    const auto b = manager->add_consumer(log.callback('b'));
    ASSERT_TRUE(manager->start(60));

    std::thread capture([&] { push_frame(*state, 1, 1); });
    entered.get_future().wait();

    const auto t0 = std::chrono::steady_clock::now();
    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Abandoned);
    EXPECT_GE(std::chrono::steady_clock::now() - t0,
              CaptureManager::kRemoveDeadline - std::chrono::milliseconds(50));

    // The stuck call ends late. The abandoned consumer gets nothing more,
    // and the other one still gets every frame.
    release.set_value();
    capture.join();
    push_frame(*state, 2, 2);
    EXPECT_EQ(calls.load(), 1);
    EXPECT_EQ(log.textures_of('b'), (std::vector<uintptr_t>{1, 2}));

    EXPECT_EQ(manager->remove_consumer(b), CaptureManager::RemoveResult::Removed);
    EXPECT_EQ(state->stops.load(), 1);
}

TEST(CaptureManager, ASwapReachesEachConsumerOnce) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);

    // Each consumer counts the frames whose generation is new to it, the
    // rule a host uses to force a keyframe. `b` skips frame 2, as a
    // consumer that decimates does.
    struct Counter {
        uint32_t handled = 0;
        int changes = 0;
    };
    Counter ca{manager->swap_generation(), 0};
    Counter cb{manager->swap_generation(), 0};
    const auto a = manager->add_consumer([&](const CapturedFrame& f) {
        if (f.swap_generation != ca.handled) {
            ca.handled = f.swap_generation;
            ++ca.changes;
        }
    });
    const auto b = manager->add_consumer([&](const CapturedFrame& f) {
        if (reinterpret_cast<uintptr_t>(f.texture) == 2) return;
        if (f.swap_generation != cb.handled) {
            cb.handled = f.swap_generation;
            ++cb.changes;
        }
    });
    ASSERT_TRUE(manager->start(60));

    push_frame(*state, 1, 1);
    state->swap_pending = true;
    push_frame(*state, 2, 2);
    push_frame(*state, 3, 3);
    push_frame(*state, 4, 4);

    EXPECT_EQ(state->swaps_consumed.load(), 1);
    EXPECT_EQ(manager->swap_generation(), 1u);
    EXPECT_EQ(ca.changes, 1);
    EXPECT_EQ(cb.changes, 1);
    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Removed);
    EXPECT_EQ(manager->remove_consumer(b), CaptureManager::RemoveResult::Removed);
}

TEST(CaptureManager, TheLastRemoveStopsTheSourceAndEndsTheManager) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);
    FrameLog log;
    const auto a = manager->add_consumer(log.callback('a'));
    ASSERT_TRUE(manager->start(60));
    EXPECT_TRUE(manager->start(60));
    EXPECT_EQ(state->starts.load(), 1);

    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Removed);
    EXPECT_EQ(state->stops.load(), 1);
    EXPECT_FALSE(manager->stop_timed_out());
    // The manager is used once.
    EXPECT_EQ(manager->add_consumer(log.callback('b')), 0u);
    EXPECT_FALSE(manager->start(60));
    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Removed);

    manager.reset();
    EXPECT_TRUE(state->destroyed);
    EXPECT_EQ(state->stops.load(), 1);
}

TEST(CaptureManager, ASourceStopThatTimesOutAbandonsTheConsumer) {
    auto state = std::make_shared<FakeState>();
    state->stop_times_out = true;
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);
    FrameLog log;
    const auto a = manager->add_consumer(log.callback('a'));
    ASSERT_TRUE(manager->start(60));

    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Abandoned);
    EXPECT_TRUE(manager->stop_timed_out());
    // The source may still run a thread: the manager never frees it.
    manager.reset();
    EXPECT_FALSE(state->destroyed);
}

TEST(CaptureManager, AStartThatFailsLeavesTheSourceStopped) {
    auto state = std::make_shared<FakeState>();
    state->start_ok = false;
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);
    FrameLog log;
    const auto a = manager->add_consumer(log.callback('a'));
    EXPECT_FALSE(manager->start(60));
    // The source never started, so the last remove does not stop it.
    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Removed);
    EXPECT_EQ(state->stops.load(), 0);
}

TEST(CaptureManager, AddRefusesAnEmptyCallbackAndAFifthConsumer) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);
    EXPECT_EQ(manager->add_consumer(nullptr), 0u);
    FrameLog log;
    std::vector<CaptureManager::ConsumerId> ids;
    for (size_t i = 0; i < CaptureManager::kMaxConsumers; ++i) {
        ids.push_back(manager->add_consumer(log.callback('x')));
        EXPECT_NE(ids.back(), 0u);
    }
    EXPECT_EQ(manager->add_consumer(log.callback('y')), 0u);
    for (const auto id : ids) {
        EXPECT_EQ(manager->remove_consumer(id), CaptureManager::RemoveResult::Removed);
    }
}

TEST(CaptureManager, AudioReachesEachConsumer) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);
    FrameLog log;
    const auto a = manager->add_consumer(log.callback('a'));
    const auto b = manager->add_consumer(log.callback('b'));
    ASSERT_TRUE(manager->start(60));

    // No consumer listens: the source has no callback and makes no audio.
    EXPECT_FALSE(source_has_audio_callback(*state));
    EXPECT_FALSE(source_has_timed_callback(*state));

    std::atomic<int> audio_a{0};
    std::atomic<int> audio_b{0};
    manager->set_audio_callback(a, [&](const float* s, uint32_t frames, uint32_t ch,
                                       uint32_t rate) {
        EXPECT_EQ(s, kSamples);
        EXPECT_EQ(frames, 2u);
        EXPECT_EQ(ch, 2u);
        EXPECT_EQ(rate, 48'000u);
        audio_a.fetch_add(1);
    });
    manager->set_audio_callback(b, [&](const float*, uint32_t, uint32_t, uint32_t) {
        audio_b.fetch_add(1);
    });
    EXPECT_TRUE(source_has_audio_callback(*state));
    for (int i = 0; i < 3; ++i) push_audio(*state);
    EXPECT_EQ(audio_a.load(), 3);
    EXPECT_EQ(audio_b.load(), 3);

    std::atomic<int64_t> timed_at_a{0};
    manager->set_timed_audio_callback(
        a, [&](const float*, uint32_t, uint32_t, uint32_t, int64_t at) { timed_at_a = at; });
    EXPECT_TRUE(source_has_timed_callback(*state));
    push_timed_audio(*state, 1234);
    EXPECT_EQ(timed_at_a.load(), 1234);

    // `a` stops listening; `b` still hears the source.
    manager->set_audio_callback(a, nullptr);
    push_audio(*state);
    EXPECT_EQ(audio_a.load(), 3);
    EXPECT_EQ(audio_b.load(), 4);
    EXPECT_TRUE(source_has_audio_callback(*state));

    // The last listener goes: the source callback goes too.
    manager->set_audio_callback(b, nullptr);
    EXPECT_FALSE(source_has_audio_callback(*state));

    // A removed consumer stops its timed audio with it.
    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Removed);
    EXPECT_FALSE(source_has_timed_callback(*state));
    EXPECT_EQ(manager->remove_consumer(b), CaptureManager::RemoveResult::Removed);
}

TEST(CaptureManager, ClearingAudioWaitsForACallInFlight) {
    auto state = std::make_shared<FakeState>();
    auto manager = make_manager(state);
    ASSERT_NE(manager, nullptr);
    FrameLog log;
    const auto a = manager->add_consumer(log.callback('a'));
    const auto b = manager->add_consumer(log.callback('b'));
    ASSERT_TRUE(manager->start(60));

    std::promise<void> entered;
    std::promise<void> release;
    std::shared_future<void> released = release.get_future().share();
    std::atomic<bool> returned{false};
    manager->set_timed_audio_callback(a, [&](const float*, uint32_t, uint32_t, uint32_t, int64_t) {
        entered.set_value();
        released.wait();
        returned = true;
    });
    // `b` keeps the source's callback set, so the manager itself must wait.
    manager->set_timed_audio_callback(b, [](const float*, uint32_t, uint32_t, uint32_t, int64_t) {});

    // A source that calls outside its own lock, so only the manager's wait
    // can hold the clear.
    CaptureSource::TimedAudioCallback source_cb;
    {
        std::lock_guard<std::mutex> lock(state->audio_mutex);
        source_cb = state->timed_cb;
    }
    std::thread audio([&] { source_cb(kSamples, 2, 2, 48'000, 1); });
    entered.get_future().wait();

    auto clearing = std::async(std::launch::async,
                               [&] { manager->set_timed_audio_callback(a, nullptr); });
    EXPECT_EQ(clearing.wait_for(std::chrono::milliseconds(100)), std::future_status::timeout);
    release.set_value();
    clearing.get();
    EXPECT_TRUE(returned);
    audio.join();

    EXPECT_EQ(manager->remove_consumer(a), CaptureManager::RemoveResult::Removed);
    EXPECT_EQ(manager->remove_consumer(b), CaptureManager::RemoveResult::Removed);
}
