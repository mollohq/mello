#pragma once
#include "capture_source.hpp"

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
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override { return width_; }
    uint32_t height() const override { return height_; }
    const char* backend_name() const override { return "WGC"; }

    bool get_cursor(CursorData& out) override;

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
    std::atomic<bool> running_{false};
    FrameCallback     callback_;

    std::mutex   cursor_mutex_;
    CursorData   cursor_;
};

} // namespace mello::video
#endif
