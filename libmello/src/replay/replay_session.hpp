#pragma once
#ifdef _WIN32
#include "replay_audio_ring.hpp"
#include "replay_video_ring.hpp"
#include "../video/capture_manager.hpp"
#include "../video/capture_source.hpp"
#include "../video/encoder.hpp"
#include <d3d11.h>
#include <wrl/client.h>
#include <array>
#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <filesystem>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>

namespace mello::audio {
class AudioPipeline;
class WasapiLoopbackCapture;
}  // namespace mello::audio

namespace mello::video {
class VideoPreprocessor;
}

namespace mello::replay {

struct ReplayConfig {
    /// The game window: the replay captures it with WGC window capture, and
    /// nothing else (plans/GAME-CLIPS.md 3.3: no monitor or desktop capture).
    void* hwnd = nullptr;
    /// The game process: its audio, through WASAPI process loopback of its
    /// process tree. 0: no game audio.
    uint32_t pid = 0;
    /// The clip quality (plan section 7): the encode height and rate.
    uint32_t height = 720;
    uint32_t fps = 60;
    uint32_t bitrate_kbps = 5000;
    /// The folder of the ring files. Empty: the OS temp folder.
    std::filesystem::path dir;
    /// Tests and the replay lane of the stream gate: capture this source in
    /// place of the window (a reference clip).
    std::optional<video::CaptureSourceDesc> source;
};

enum class ReplayState {
    Recording,  ///< frames come in
    Waiting,    ///< the game draws nothing now (minimized, a loading screen)
    Failed,     ///< the capture, the encoder or a ring failed: the replay is off
};

struct ReplayClip {
    uint64_t duration_us = 0;
    uint32_t width = 0;
    uint32_t height = 0;
    uint64_t bytes = 0;
    bool     has_game_audio = false;
    /// The poster came from a frame after the press. False: from the last IDR
    /// of the clip (a minimized game delivers no frame).
    bool     poster_live = false;
};

/// The game clip replay buffer (plans/GAME-CLIPS.md 3, 5): a consumer of a
/// CaptureManager with its own preprocessor and recording encoder, the video
/// and game audio rings, and the capture of a clip into an MP4 and a poster.
///
/// This version has its own capture manager on the game window. The shared
/// capture with a running stream of the same game (plan 3.5, items 4 and 5)
/// comes next.
///
/// Threads:
///  - The capture thread calls the frame callback: decimate, convert into the
///    NV12 ring, enqueue (2 deep, newest wins), and the poster copy (a
///    submit, no wait, plan F6).
///  - The encode thread encodes and hands each access unit to the video ring.
///  - The loopback thread hands the game audio to the audio ring.
///  - `capture` runs on the caller's worker thread, never the command loop.
///    `stop` and `state` run on any thread.
class ReplaySession : public std::enable_shared_from_this<ReplaySession> {
public:
    /// Starts the capture, the encoder, the rings and the game audio. Null
    /// on failure; `error` says why, for the HUD ("not available").
    static std::shared_ptr<ReplaySession> start(const ReplayConfig& config, std::string* error);

    ~ReplaySession();
    ReplaySession(const ReplaySession&) = delete;
    ReplaySession& operator=(const ReplaySession&) = delete;

    /// Stops everything and deletes the ring files. A capture that runs now
    /// keeps its windows and finishes. The owner calls it before it drops
    /// the session: when the capture thread does not stop in time, the
    /// session then keeps itself alive for good, as an abandoned stream
    /// pipeline does.
    void stop();

    ReplayState state() const;

    /// Writes the last `seconds` of the game as an MP4 at `mp4` and a JPEG
    /// poster at `poster`: the video from the last keyframe at or before the
    /// start, the game audio, and the voice of `voice` (the crew mix with
    /// m3llo's mixer volume, and the mic). Blocks for the mux, about a
    /// second. False on failure.
    bool capture(float seconds, audio::AudioPipeline* voice, const std::filesystem::path& mp4,
                 const std::filesystem::path& poster, ReplayClip* out);

    struct Stats {
        uint64_t frames_captured = 0;
        uint64_t frames_encoded = 0;
        uint64_t encode_queue_drops = 0;
        ReplayVideoRing::Stats video_ring;
        ReplayAudioRing::Stats audio_ring;
    };
    Stats stats() const;

    uint32_t encode_width() const { return enc_w_; }
    uint32_t encode_height() const { return enc_h_; }

private:
    struct Key {
        explicit Key() = default;
    };

public:
    /// Use `start`. Out of line: the members of incomplete type are freed there.
    explicit ReplaySession(Key);

private:
    struct Job {
        ID3D11Texture2D* texture = nullptr;  // an NV12 ring slot
        uint64_t capture_us = 0;
    };
    static constexpr size_t kQueueCap = 2;

    bool start_impl(const ReplayConfig& config, std::string* error);
    void on_frame(const video::CapturedFrame& frame);
    void encode_loop();
    void on_game_audio(const int16_t* samples, size_t count);
    /// The capture thread copies the frame for the poster (plan F6).
    void copy_poster_frame(ID3D11Texture2D* texture);
    bool write_live_poster(const std::filesystem::path& poster, std::chrono::milliseconds wait);

    ReplayConfig config_;
    std::shared_ptr<video::CaptureManager> manager_;
    video::CaptureManager::ConsumerId consumer_id_ = 0;
    std::unique_ptr<video::VideoPreprocessor> preprocessor_;
    std::unique_ptr<video::Encoder> encoder_;
    uint32_t enc_w_ = 0;
    uint32_t enc_h_ = 0;

    ReplayVideoRing video_ring_;
    ReplayAudioRing audio_ring_;
    std::unique_ptr<audio::WasapiLoopbackCapture> loopback_;
    bool has_game_audio_ = false;

    // Capture thread only.
    uint64_t next_deadline_us_ = 0;
    uint32_t handled_swap_generation_ = 0;

    // Capture thread to encode thread.
    std::mutex queue_mutex_;
    std::condition_variable queue_cv_;
    std::array<Job, kQueueCap> queue_{};
    size_t queue_head_ = 0;
    size_t queue_count_ = 0;
    std::atomic<bool> keyframe_wanted_{false};
    std::thread encode_thread_;
    std::atomic<bool> running_{false};
    std::atomic<bool> failed_{false};

    // The poster: the capture thread copies the next frame into a staging
    // texture when `poster_wanted_` is set; `capture` maps it.
    std::atomic<bool> poster_wanted_{false};
    std::mutex poster_mutex_;
    std::condition_variable poster_cv_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> poster_staging_;  // poster_mutex_
    bool poster_ready_ = false;                                // poster_mutex_

    // Serializes stop and capture.
    std::mutex control_mutex_;

    std::atomic<uint64_t> frames_captured_{0};
    std::atomic<uint64_t> frames_encoded_{0};
    std::atomic<uint64_t> queue_drops_{0};
};

}  // namespace mello::replay
#endif
