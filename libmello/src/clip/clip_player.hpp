#pragma once

// The game clip player (CLIPS.md 15.9): an MP4 that plays from its
// first bytes, read through a callback (mello-core's range reader, or a file).
// The same on every platform (clip_player.cpp):
//
//   read callback --> m3llo's MP4 demux (the player's own thread)
//       H.264 --> frame source --> the stream viewer's decode and present threads
//       Opus  --> libopus --> ClipAudioStream --> the audio render thread (the clock)
//
// The audio is the master clock: a frame's render time is now plus its time
// minus the clip position, where the position is the PCM that played less
// the output latency. A read that waits is a stall: the audio pauses, the
// picture holds, the state says buffering, and play goes on when 1 s of
// sound waits again (0.3 s at the start and after a seek).
//
// Threads: start, set_native_frame_callback, set_paused, seek, state and stop
// run on the owner's thread (any one at a time).

#include "../audio/clip_audio_stream.hpp"
#include "../video/decoder.hpp"
#include "../video/video_viewer.hpp"

#include <cstdint>
#include <functional>
#include <memory>

namespace mello::clip {

/// A read returns the bytes read (0 at the end), kReadWait when the bytes
/// are not there yet (the player asks again), or a negative error.
constexpr int64_t kReadWait = -2;
using ReadFn = std::function<int64_t(uint64_t offset, uint8_t* buf, uint32_t len)>;

struct ClipPlayerOptions {
    /// The size of the MP4 in bytes.
    uint64_t length = 0;
    /// Called on the player's demux thread. It must not block for long:
    /// bytes that are not there yet return kReadWait.
    ReadFn read;
    /// Gives the clip's sound to the audio render thread, null at the stop
    /// (production: AudioPipeline::set_clip_stream).
    std::function<void(std::shared_ptr<audio::ClipAudioStream>)> attach_audio;
    /// The speakers' output latency in µs: the position is what is heard.
    std::function<int64_t()> output_latency_us;
    /// Tests: a decoder on the player's device in place of the factory's,
    /// and a CPU frame callback.
    std::function<std::unique_ptr<video::Decoder>(const video::GraphicsDevice&)> make_decoder;
    video::ViewerPipeline::FrameCallback on_cpu_frame;
};

struct ClipPlayerState {
    int64_t position_us = 0;
    int64_t duration_us = 0;
    uint32_t width = 0;
    uint32_t height = 0;
    /// Play waits: the sound ran out (or, in a clip without sound, a read
    /// waits for bytes). See `reports_buffering`.
    bool buffering = false;
    bool paused = false;
    /// The last frame was shown.
    bool ended = false;
    /// The clip cannot play (no video, a read error, no decoder).
    bool failed = false;
};

/// The `buffering` of the state. With sound, the sound's stall rule says when
/// play waits: a read that waits while the sound plays on is the source
/// reading ahead, not a stall (CLIPS.md 15.13). Without sound, a
/// read that waits is the only sign.
inline bool reports_buffering(bool stalled, bool has_audio, bool read_waits, bool ended) {
    return (stalled || (!has_audio && read_waits)) && !ended;
}

class ClipPlayer {
public:
    /// Null without a read callback or a length.
    static std::unique_ptr<ClipPlayer> start(ClipPlayerOptions options);
    virtual ~ClipPlayer() = default;

    virtual void set_native_frame_callback(video::ViewerPipeline::NativeFrameCallback cb) = 0;
    /// macOS and Linux: each frame as RGBA on the present thread, at its
    /// render time. They have no native presenter: the client shows the RGBA
    /// as it shows a watched stream's. Windows does not call it (the native
    /// presenter), so its viewer makes no CPU copy.
    virtual void set_frame_callback(video::ViewerPipeline::FrameCallback cb) = 0;
    virtual void set_paused(bool paused) = 0;
    /// Plays from `position_us` (from the key frame before it).
    virtual void seek(int64_t position_us) = 0;
    virtual ClipPlayerState state() const = 0;
    /// Stops the threads and takes the sound out of the mix. It joins: run it
    /// off any loop that must stay responsive.
    virtual void stop() = 0;
};

}  // namespace mello::clip
