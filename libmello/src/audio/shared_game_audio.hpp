#pragma once
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <utility>
#include <vector>

namespace mello::audio {

/// The platform's capture of one game's sound (shared_game_audio_win.cpp,
/// shared_game_audio_mac.mm). It stops when it goes.
class GameAudioCapture {
public:
    virtual ~GameAudioCapture() = default;
};

/// The game audio of one game, given to every consumer that needs it: the
/// stream host's audio pipeline and the game clip replay (12-STREAMING.md
/// 3.7, F7). One capture of the game's process tree, as the game has one
/// video capture:
///   Windows: WASAPI process loopback of the process tree.
///   macOS:   a Core Audio process tap of the game's processes (macOS 14.2).
///            ScreenCaptureKit audio is the whole mix of the Mac
///            (CLIPS.md 15.12a), so it is not used.
///
/// Never the whole mix: it records the crew voice and every other app
/// (CLIPS.md 15.12). Where the platform cannot give it `open` returns null,
/// and the stream host keeps its own capture as before.
///
/// Threads: the capture's thread calls each consumer, in the order of `add`,
/// with 48 kHz stereo int16 (the count is in samples) and the capture time
/// of the first sample on the steady clock (0: unknown, the consumer uses
/// the arrival). `add` and `remove` on any thread, never from inside a
/// consumer; `remove` waits for a call that runs now. The capture stops when
/// the object goes, so each consumer holds it by `shared_ptr`.
class SharedGameAudio {
public:
    using Callback = std::function<void(const int16_t* samples, size_t count, int64_t capture_us)>;
    using ConsumerId = uint32_t;

    /// Starts the capture of `pid`'s process tree. Null when the platform
    /// cannot give it.
    static std::shared_ptr<SharedGameAudio> open(uint32_t pid);
    /// Tests: no device; `deliver_for_test` stands in for the capture thread.
    static std::shared_ptr<SharedGameAudio> for_test(uint32_t pid);

    ~SharedGameAudio();
    SharedGameAudio(const SharedGameAudio&) = delete;
    SharedGameAudio& operator=(const SharedGameAudio&) = delete;

    uint32_t pid() const { return pid_; }
    /// Adds a consumer: the next packet reaches it. 0 for an empty callback.
    ConsumerId add(Callback callback);
    /// Takes a consumer out. After it returns, the consumer gets no call.
    void remove(ConsumerId id);

    void deliver_for_test(const int16_t* samples, size_t count, int64_t capture_us = 0) {
        deliver(samples, count, capture_us);
    }
    /// The capture's thread: the next PCM for every consumer.
    void deliver(const int16_t* samples, size_t count, int64_t capture_us);

private:
    struct Key {
        explicit Key() = default;
    };

public:
    SharedGameAudio(Key, uint32_t pid);

private:
    uint32_t pid_ = 0;
    std::unique_ptr<GameAudioCapture> capture_;
    // Held for the whole fan-out, so a remove waits for a call that runs.
    std::mutex mutex_;
    std::vector<std::pair<ConsumerId, Callback>> consumers_;
    ConsumerId next_id_ = 1;
};

/// The platform's `open`: the capture of `pid`'s process tree delivering
/// into `shared`. Null when there is none.
std::unique_ptr<GameAudioCapture> open_game_audio_capture(uint32_t pid, SharedGameAudio* shared);

}  // namespace mello::audio
