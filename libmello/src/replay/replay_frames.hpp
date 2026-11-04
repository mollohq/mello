#pragma once

// The platform part of the game clip replay (CLIPS.md 15.3): the replay's own
// capture of the game window, the frames from the capture thread to the
// recording encoder, and the poster. The rest of the replay (replay_session)
// is the same on every platform.
//
//   Windows: WGC window capture, the D3D11 preprocessor into the replay's
//            NV12 ring, a staging texture for the poster (replay_frames_win.cpp).
//   macOS:   ScreenCaptureKit on the window, at the clip quality's size; the
//            CVPixelBuffer goes to VideoToolbox as it is (replay_frames_mac.mm).

#include "../video/capture_manager.hpp"
#include "../video/capture_source.hpp"
#include "../video/encoder.hpp"

#include <chrono>
#include <cstdint>
#include <filesystem>
#include <memory>
#include <string>

namespace mello::replay {

/// The frames of one capture manager, from the capture thread to the encoder
/// and the poster. Made on the manager's device for one encode size.
class ReplayFrames {
public:
    virtual ~ReplayFrames() = default;

    /// Capture thread: the frame as the encoder takes it, held until
    /// `release`. Null: drop the frame. It never waits on the GPU.
    virtual void* take(const video::CapturedFrame& frame) = 0;
    /// Any thread: the end of a frame from `take`, encoded or not.
    virtual void release(void* frame) = 0;
    /// Encode thread. `unit_us`: the capture time of the unit in `out`, which
    /// is an earlier frame's when the encoder keeps frames in flight.
    virtual bool encode(video::Encoder& encoder, void* frame, uint64_t capture_us,
                        video::EncodedPacket& out, uint64_t* unit_us) = 0;

    /// Capture thread: keep this frame for the poster (12-STREAMING.md 3.7,
    /// F6: a copy that is submitted, no wait).
    virtual void keep_poster(const video::CapturedFrame& frame) = 0;
    /// The capture's worker thread: the kept frame as a JPEG. False when no
    /// frame was kept within `wait`.
    virtual bool write_poster(const std::filesystem::path& jpeg, std::chrono::milliseconds wait) = 0;
    /// Forgets a kept frame (the start of a capture).
    virtual void drop_poster() = 0;
};

/// The frames of `manager` at the encode size. Null with `error` set.
std::unique_ptr<ReplayFrames> make_replay_frames(const video::CaptureManager& manager, uint32_t enc_w,
                                                 uint32_t enc_h, std::string* error);

/// The process that owns `window` (an HWND, a CGWindowID). 0 when unknown.
uint32_t window_owner_pid(void* window);

/// The main window of the game `pid` (an HWND, a CGWindowID), as the
/// stream's process capture finds it. Null when it has none.
void* game_main_window(uint32_t pid);

/// The replay's own capture of the game window, on a new device: the window
/// and nothing else, never a monitor (CLIPS.md 15.3). `height` is the clip
/// quality's height (macOS captures at it). Null with `error` set.
std::shared_ptr<video::CaptureManager> make_window_capture(void* window, uint32_t height, std::string* error);

/// A capture of `desc` on a new device: the reference clip of the tests and
/// the replay lane.
std::shared_ptr<video::CaptureManager> make_source_capture(const video::CaptureSourceDesc& desc,
                                                           std::string* error);

}  // namespace mello::replay
