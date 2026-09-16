#pragma once
#include "graphics_device.hpp"
#include "present_delay.hpp"
#include <cstdint>
#include <functional>
#include <memory>
#include <string>
#include <vector>

#ifdef _WIN32
#include <d3d11.h>
#endif

namespace mello::video {

enum class CaptureMode {
    Monitor,
    Window,
    Process,
};

struct CaptureSourceDesc {
    CaptureMode mode;
    union {
        uint32_t monitor_index;
        void*    hwnd;
        uint32_t pid;
    };
    /// Monitor mode only: capture with Windows Graphics Capture instead of
    /// DXGI desktop duplication. Used by the DXGI vs WGC benchmark.
    bool prefer_wgc = false;
    /// Process mode only: the caller allows the game capture hook for this
    /// game. False keeps the ladder on screen capture, whatever the game is.
    bool allow_hook = false;
};

struct CursorData {
    int32_t  x = 0;
    int32_t  y = 0;
    bool     visible = true;
    bool     shape_changed = false;
    uint16_t shape_w = 0;
    uint16_t shape_h = 0;
    std::vector<uint8_t> shape_rgba;
};

/// What the capture is doing right now, for the person streaming and for the
/// people watching. Everything except `Capturing` means the viewer is looking
/// at a still picture, and both ends should be told why rather than shown a
/// black rectangle.
enum class CaptureState : uint32_t {
    Capturing = 0,
    /// The game is minimized. Nothing can capture a minimized window, and the
    /// stream carries on as soon as it comes back.
    WaitingMinimized = 1,
    /// The game is up but has drawn nothing yet: it is loading, or the person
    /// has not reached it.
    WaitingForGame = 2,
    /// Every method failed, with proof that the game is drawing.
    Failed = 3,
};

const char* capture_state_name(CaptureState state);

class CaptureSource {
public:
#ifdef _WIN32
    using FrameCallback = std::function<void(ID3D11Texture2D* texture, uint64_t timestamp_us)>;
#else
    using FrameCallback = std::function<void(void* texture, uint64_t timestamp_us)>;
#endif

    /// Receives interleaved float PCM for system ("game") audio.
    /// `frame_count` is frames, not samples: one frame holds `channels` values.
    using AudioCallback = std::function<void(const float* samples, uint32_t frame_count,
                                            uint32_t channels, uint32_t sample_rate)>;

    virtual ~CaptureSource() = default;

    virtual bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) = 0;
    virtual bool start(uint32_t target_fps, FrameCallback callback) = 0;
    virtual void stop() = 0;

    virtual uint32_t width()  const = 0;
    virtual uint32_t height() const = 0;
    virtual const char* backend_name() const = 0;

    virtual bool get_cursor(CursorData& out) { (void)out; return false; }

    /// Route system audio captured alongside video to `cb`, replacing any
    /// previous callback. Safe to call before or after `start()`.
    ///
    /// Only backends that capture audio and video from one OS stream implement
    /// this (ScreenCaptureKit). On Windows game audio comes from a separate
    /// WASAPI loopback device, so the default is a no-op.
    virtual void set_audio_callback(AudioCallback cb) { (void)cb; }
    // Backends with runtime source/backend switching can raise a swap event.
    // The pipeline consumes this to force a keyframe and accelerate recovery.
    virtual bool consume_swap_event() { return false; }

    /// True when the backend has stopped for good (duplication rebuild gave up,
    /// capture item closed) or, for ProcessCapture, when every capture method
    /// failed to deliver a first frame. Silence alone never sets this: a static
    /// screen delivers no frames and is healthy.
    virtual bool failed() const { return false; }

    /// Short history of capture method changes and their reasons, for logs and
    /// host telemetry. Empty for backends that never change method.
    virtual std::string method_history() const { return {}; }

    /// What to tell the user about this capture. See CaptureState.
    virtual CaptureState state() const { return CaptureState::Capturing; }

    /// True when this backend works but the game has drawn nothing yet.
    ///
    /// Only the hook can tell: it counts the game's presents from inside the
    /// game. Silence then says nothing about the capture method, and moving the
    /// ladder on would trade the best method for one that shows the desktop.
    /// Every other backend answers false, because none of them can know.
    virtual bool waiting_for_the_game() const { return false; }

    /// True when `stop()` gave up waiting for its own thread and detached it.
    ///
    /// The thread may still touch this object, so the owner must never destroy
    /// it: leak it instead. `IDXGIOutputDuplication::AcquireNextFrame` can block
    /// inside the display driver for as long as another application holds the
    /// output in exclusive fullscreen, whatever timeout it was given. That is
    /// what froze a host on 2026-09-15 and what a 2026-09-16 dump confirmed.
    virtual bool stop_timed_out() const { return false; }

    /// Where to record present-to-capture delay. Backends that can measure it
    /// (DXGI, WGC) record every delivered frame. The histogram outlives the
    /// capture source.
    virtual void set_present_delay_histogram(PresentDelayHistogram* hist) { (void)hist; }
};

/// Smallest frame the hardware encoders accept. NVENC H.264 is 145x49, and AMF
/// and QSV are comparable. A capture source below this cannot be encoded at all.
static constexpr uint32_t kMinEncodeWidth  = 145;
static constexpr uint32_t kMinEncodeHeight = 49;

#ifdef _WIN32
/// True when a window can carry a stream on its own.
///
/// A window that is too small to encode cannot, and neither can a proxy window:
/// a Direct3D 9 game in exclusive fullscreen leaves a tiny `D3DProxyWindow`
/// behind, and that is what a window picker lists. Measured on 2026-09-16:
/// picking Unigine Heaven in the window list gave a 160x28 proxy and the stream
/// refused to start.
bool window_is_capturable(uint32_t client_width, uint32_t client_height,
                          const std::string& window_class);

/// Returns the source to capture for what the user picked.
///
/// The user picks a window; the client picks the method. A window that cannot
/// carry a stream becomes its process, which runs the whole capture ladder and
/// can see a fullscreen game. Every other choice is returned unchanged.
CaptureSourceDesc resolve_capture_target(const CaptureSourceDesc& desc);
#endif

std::unique_ptr<CaptureSource> create_capture_source(const CaptureSourceDesc& desc);

} // namespace mello::video
