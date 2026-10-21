#pragma once
#include "audio/audio_pipeline.hpp"
#include "video/video_pipeline.hpp"
#include <atomic>
#include <cstdint>
#include <memory>
#include <string>
#include <mutex>

namespace mello {

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
};

} // namespace mello
