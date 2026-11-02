#pragma once
#include "audio/audio_pipeline.hpp"
#include "audio/stream_audio_link.hpp"
#include "video/video_pipeline.hpp"
#include <atomic>
#include <cstdint>
#include <memory>
#include <string>
#include <mutex>

namespace mello {

namespace replay {
class ReplaySession;
}

class Context {
public:
    Context() = default;
    ~Context() = default;

    bool initialize();
    void shutdown();

    audio::AudioPipeline& audio() { return audio_; }
    video::VideoPipeline& video() { return video_; }

    // Dropped voice slot packets of every voice peer made with this context
    // (MelloDebugStats.rx_unmapped_dropped). Shared, so a peer that outlives
    // the context still counts into valid memory.
    std::shared_ptr<std::atomic<uint64_t>> rx_unmapped_dropped_counter() const {
        return rx_unmapped_dropped_;
    }

    /// Test only (mello_test_set_reference_clip): the clip a host with
    /// MELLO_CAPTURE_REFERENCE plays. Any thread.
    void set_reference_clip(const video::ReferenceClip& clip) {
        std::lock_guard<std::mutex> lock(reference_mutex_);
        reference_clip_ = clip;
    }
    video::ReferenceClip reference_clip() const {
        std::lock_guard<std::mutex> lock(reference_mutex_);
        return reference_clip_;
    }

    /// Streaming-quality stage 7: the link between this context's stream
    /// host audio pipeline and its stream host peer (stream_audio_link.hpp).
    std::shared_ptr<audio::StreamAudioLink> stream_audio_link() const { return stream_audio_link_; }

    /// The game clip replay (F8), one at a time (plans/GAME-CLIPS.md 4.2).
    /// A capture holds its own reference, so a stop during a capture lets
    /// the capture finish. Any thread.
    std::shared_ptr<replay::ReplaySession> replay() const {
        std::lock_guard<std::mutex> lock(replay_mutex_);
        return replay_;
    }
    /// Sets the replay and returns the one before it, for the caller to stop.
    std::shared_ptr<replay::ReplaySession> exchange_replay(std::shared_ptr<replay::ReplaySession> next) {
        std::lock_guard<std::mutex> lock(replay_mutex_);
        replay_.swap(next);
        return next;
    }

#ifdef _WIN32
    /// The capture of the running stream host, so a replay of the same game
    /// that starts later joins it (plans/GAME-CLIPS.md 3.5). `key` is the
    /// game (ReplaySession::game_key). Weak: the stream owns its capture.
    struct StreamCapture {
        std::weak_ptr<video::CaptureManager> manager;
        uint32_t key = 0;
        uint32_t fps = 0;
    };
    void set_stream_capture(StreamCapture capture) {
        std::lock_guard<std::mutex> lock(replay_mutex_);
        stream_capture_ = std::move(capture);
    }
    StreamCapture stream_capture() const {
        std::lock_guard<std::mutex> lock(replay_mutex_);
        return stream_capture_;
    }
#endif

    void set_error(const std::string& error);
    const char* get_error() const;

    // Public for SEH wrapper in context.cpp (Windows crash guard)
    bool initialize_inner();

private:
    audio::AudioPipeline audio_;
    video::VideoPipeline video_;
    std::string last_error_;
    mutable std::mutex error_mutex_;
    video::ReferenceClip reference_clip_{};
    mutable std::mutex reference_mutex_;
    std::shared_ptr<std::atomic<uint64_t>> rx_unmapped_dropped_ =
        std::make_shared<std::atomic<uint64_t>>(0);
    std::shared_ptr<audio::StreamAudioLink> stream_audio_link_ =
        std::make_shared<audio::StreamAudioLink>();
    std::shared_ptr<replay::ReplaySession> replay_;
#ifdef _WIN32
    StreamCapture stream_capture_;
#endif
    mutable std::mutex replay_mutex_;  // replay_ and stream_capture_
};

} // namespace mello
