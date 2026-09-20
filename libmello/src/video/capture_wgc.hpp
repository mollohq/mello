#pragma once
#include "capture_source.hpp"
#include "process_liveness.hpp"

#ifdef _WIN32
#include <winrt/Windows.Graphics.Capture.h>
#include <winrt/Windows.Graphics.DirectX.Direct3D11.h>
#include <wrl/client.h>
#include <thread>
#include <atomic>
#include <mutex>

namespace mello::video {

class WgcCapture : public CaptureSource {
public:
    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    /// Capture a whole monitor with Windows Graphics Capture. Used by the
    /// process capture ladder as a fallback when window capture fails. It does
    /// not see exclusive-fullscreen content either.
    bool initialize_monitor(const GraphicsDevice& device, HMONITOR monitor);
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override { return width_; }
    uint32_t height() const override { return height_; }
    const char* backend_name() const override { return monitor_ ? "WGC-Monitor" : "WGC"; }

    bool get_cursor(CursorData& out) override;
    bool failed() const override { return closed_.load(std::memory_order_relaxed); }

    // No monitor thread here, so the owning process is polled lazily on the
    // ~1 Hz stats call. A closed capture item alone is not enough to end a
    // stream: a destroyed window can mean a mode change, while a dead owning
    // process is final.
    bool target_exited() const override {
        target_liveness_.refresh();
        return target_liveness_.exited();
    }

    void set_present_delay_histogram(PresentDelayHistogram* hist) override { delay_hist_ = hist; }

private:
    void on_frame_arrived(
        winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool const& pool,
        winrt::Windows::Foundation::IInspectable const&
    );

    winrt::Windows::Graphics::Capture::GraphicsCaptureItem    item_{nullptr};
    winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool frame_pool_{nullptr};
    winrt::Windows::Graphics::Capture::GraphicsCaptureSession session_{nullptr};

    Microsoft::WRL::ComPtr<ID3D11Device>        device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;

    uint32_t          width_  = 0;
    uint32_t          height_ = 0;
    bool              monitor_ = false;
    // What the capture item was made from, kept so start() can make it again.
    // stop() closes the item, and the capture ladder stops a backend and
    // starts it again when a better method turns out to be unavailable.
    HWND              source_hwnd_ = nullptr;      // window capture
    HMONITOR          source_monitor_ = nullptr;   // monitor capture
    std::atomic<bool> running_{false};
    // Set by the capture item's Closed event: the window or monitor is gone.
    std::atomic<bool> closed_{false};
    PresentDelayHistogram* delay_hist_ = nullptr;
    // Process that owns hwnd_, tracked so a quit game ends the stream instead
    // of pausing it forever. Resolved once at initialize; the handle pins the
    // exact process object against pid reuse. Empty for monitor capture, which
    // has no owning process.
    mutable ProcessLiveness target_liveness_;
    winrt::event_token closed_token_{};
    FrameCallback     callback_;

    std::mutex   cursor_mutex_;
    CursorData   cursor_;

    // Rate throttle: WGC fires at compositor rate (e.g. 144 Hz) regardless of
    // the encode target. An interval accumulator delivers exactly target_fps
    // on average while keeping phase jitter to one vsync quantum.
    std::mutex   throttle_mutex_;
    uint32_t     target_fps_ = 60;
    double       frame_credit_us_ = 0.0;
    uint64_t     last_frame_us_ = 0;
    bool         throttle_primed_ = false;
};

} // namespace mello::video
#endif
