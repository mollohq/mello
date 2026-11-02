#pragma once
#ifdef _WIN32
#include "replay_audio_ring.hpp"
#include "replay_load.hpp"
#include "replay_video_ring.hpp"
#include "../video/capture_manager.hpp"
#include "../video/capture_reference.hpp"
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
#include <vector>

namespace mello::audio {
class AudioPipeline;
class SharedGameAudio;
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
    /// The game audio capture of `pid` that the stream host shares (F7, the
    /// context's registry). Null: the replay opens one for itself.
    std::shared_ptr<audio::SharedGameAudio> game_audio;
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
    Paused,     ///< overload: the replay gave up its frames first (F3)
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
/// Alone, the replay has its own capture manager on the game window (WGC
/// window). While the user streams the same game, it is a second consumer of
/// the stream's capture manager, so the game has one capture (plan 3.2):
///  - The stream starts exactly as without the replay (F2, Q18). Then
///    `follow_stream` stops the replay's own capture and attaches the replay
///    to the stream's manager, on that manager's device.
///  - The replay takes the frames of the hook and of WGC window. It drops the
///    frames of a monitor method (WGC monitor, DXGI) and waits (3.3, Q4).
///  - It records at the stream's rate, never above it (F1).
///  - Before the stream stops, `leave_stream` puts the replay back on its own
///    WGC window capture. The stream's capture, the hook too, then stops with
///    the stream (Q19).
/// The encode size stays the same over a switch: the preprocessor fits each
/// source into it, so a clip has one picture size. Each switch starts a new
/// segment with an IDR.
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

    /// The key of the game that a capture shows: its pid, kReferenceKey for
    /// the reference clip, 0 for a monitor (never the same game).
    static constexpr uint32_t kReferenceKey = 0xFFFFFFFFu;
    uint32_t game_key() const { return game_key_; }

    /// A stream of `stream_key` started on `manager` at `stream_fps`. When it
    /// is this replay's game, the replay moves onto that manager. Call it
    /// after the stream's own start, on the stream's control thread. A
    /// failure turns only the replay off (F5).
    void follow_stream(const std::shared_ptr<video::CaptureManager>& manager, uint32_t stream_key,
                       uint32_t stream_fps);
    /// The stream that the replay follows stops: the replay goes back to its
    /// own capture first. Call it before the stream removes its consumer.
    /// Does nothing when the replay follows no stream.
    void leave_stream();
    /// True while the replay is a consumer of a stream's capture manager.
    bool following_stream() const { return following_.load(std::memory_order_acquire); }
    /// True while the replay is a consumer of `manager`, a stream's.
    bool follows(const video::CaptureManager* manager) const {
        return following_stream() && manager && std::atomic_load(&manager_).get() == manager;
    }

    /// F3: the stream that the replay follows is overloaded. The replay gives
    /// up one step (30 fps, then a pause) and returns true; false when it is
    /// paused already, and the stream then gives up its own quality. Called
    /// on the stream's encode thread; fast (a short lock, no wait).
    bool shed_for_stream();
    ReplayLoad::Level load_level() const { return load_.level(); }

    /// Writes the last `seconds` of the game as an MP4 at `mp4` and a JPEG
    /// poster at `poster`: the video from the last keyframe at or before the
    /// start, the game audio, and the voice of `voice` (the crew mix with
    /// m3llo's mixer volume, and the mic). Blocks for the mux, about a
    /// second. False on failure.
    bool capture(float seconds, audio::AudioPipeline* voice, const std::filesystem::path& mp4,
                 const std::filesystem::path& poster, ReplayClip* out);

    struct Stats {
        uint64_t frames_captured = 0;
        uint64_t frames_offered = 0;
        uint64_t frames_encoded = 0;
        uint64_t encode_queue_drops = 0;
        ReplayVideoRing::Stats video_ring;
        ReplayAudioRing::Stats audio_ring;
    };
    Stats stats() const;

    uint32_t encode_width() const { return enc_w_; }
    uint32_t encode_height() const { return enc_h_; }

    /// The reference source's clock (the replay lane of the stream gate).
    /// False when the replay captures something else.
    bool reference_info(video::ReferenceInfo& out) const;

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
    /// The replay's own capture: WGC on the game window (or the test source).
    std::shared_ptr<video::CaptureManager> make_own_capture(std::string* error) const;
    /// control_mutex_ held. Builds the preprocessor and the encoder on
    /// `manager`'s device at `fps`, starts the encode thread and attaches to
    /// `manager`. Starts the manager when `own`.
    bool open_capture(std::shared_ptr<video::CaptureManager> manager, bool own, uint32_t fps,
                      std::string* error);
    /// control_mutex_ held. Detaches from the manager, stops the encode
    /// thread and frees the preprocessor and the encoder. False when the
    /// capture thread did not leave in time: the session is then abandoned.
    bool close_capture();
    void on_frame(const video::CapturedFrame& frame);
    void encode_loop();
    /// Encode thread, every 60 frames: the replay's own overload (F3).
    void check_own_load();
    void on_game_audio(const int16_t* samples, size_t count);
    /// The reference source's audio, at its capture times. The source's
    /// audio thread only.
    void on_timed_audio(const float* samples, uint32_t frames, uint32_t channels, uint32_t rate,
                        int64_t capture_us);
    std::vector<int16_t> timed_pcm_;  // on_timed_audio's thread
    /// The capture thread copies the frame for the poster (plan F6).
    void copy_poster_frame(ID3D11Texture2D* texture);
    bool write_live_poster(const std::filesystem::path& poster, std::chrono::milliseconds wait);

    ReplayConfig config_;
    uint32_t game_key_ = 0;
    // The capture the replay is attached to (control_mutex_ for writes).
    std::shared_ptr<video::CaptureManager> manager_;
    video::CaptureManager::ConsumerId consumer_id_ = 0;
    std::unique_ptr<video::VideoPreprocessor> preprocessor_;
    std::unique_ptr<video::Encoder> encoder_;
    uint32_t enc_w_ = 0;  // fixed at start
    uint32_t enc_h_ = 0;
    uint32_t encode_fps_ = 0;
    std::atomic<bool> following_{false};
    std::atomic<bool> stopped_{false};
    // The frames now come from a monitor method of the stream: dropped.
    std::atomic<bool> monitor_frames_{false};
    // F3: 30 fps, then a pause, under overload. Kept over a switch.
    ReplayLoad load_;

    // Encode thread only: the rate the encoder runs at now, the frames since
    // the last keyframe (a 1 s GOP at any rate), and the window of the
    // replay's own overload check.
    uint32_t applied_fps_ = 0;
    uint32_t frames_since_key_ = 0;
    double encode_ms_mean_ = 0.0;
    uint64_t window_offered_ = 0;
    uint64_t window_drops_ = 0;

    ReplayVideoRing video_ring_;
    ReplayAudioRing audio_ring_;
    std::shared_ptr<audio::SharedGameAudio> game_audio_;
    uint32_t game_audio_consumer_ = 0;
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

    // Serializes stop, the follow and leave switches, and the start of a capture.
    std::mutex control_mutex_;

    std::atomic<uint64_t> frames_captured_{0};
    std::atomic<uint64_t> frames_offered_{0};  // after decimation, to the encode queue
    std::atomic<uint64_t> frames_encoded_{0};
    std::atomic<uint64_t> queue_drops_{0};
};

}  // namespace mello::replay
#endif
