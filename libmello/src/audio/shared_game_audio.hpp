#pragma once
#ifdef _WIN32
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <utility>
#include <vector>

namespace mello::audio {

class WasapiLoopbackCapture;

/// The game audio of one game, given to every consumer that needs it: the
/// stream host's audio pipeline and the game clip replay (12-STREAMING.md
/// 3.7, F7). One WASAPI process loopback of the game's process tree, as the
/// game has one video capture.
///
/// Never endpoint loopback: it records the crew voice and every other app
/// (CLIPS.md 15.12). On a Windows without process loopback `open` returns null,
/// and the stream host keeps its own capture as before.
///
/// Threads: the loopback's capture thread calls each consumer, in the order
/// of `add`, with 48 kHz stereo int16 (the count is in samples). `add` and
/// `remove` on any thread, never from inside a consumer; `remove` waits for
/// a call that runs now. The capture stops when the object goes, so each
/// consumer holds it by `shared_ptr`.
class SharedGameAudio {
public:
    using Callback = std::function<void(const int16_t* samples, size_t count)>;
    using ConsumerId = uint32_t;

    /// Starts process loopback of `pid`'s process tree. Null when Windows
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

    void deliver_for_test(const int16_t* samples, size_t count) { deliver(samples, count); }

private:
    struct Key {
        explicit Key() = default;
    };

public:
    SharedGameAudio(Key, uint32_t pid);

private:
    void deliver(const int16_t* samples, size_t count);

    uint32_t pid_ = 0;
    std::unique_ptr<WasapiLoopbackCapture> capture_;
    // Held for the whole fan-out, so a remove waits for a call that runs.
    std::mutex mutex_;
    std::vector<std::pair<ConsumerId, Callback>> consumers_;
    ConsumerId next_id_ = 1;
};

}  // namespace mello::audio
#endif
