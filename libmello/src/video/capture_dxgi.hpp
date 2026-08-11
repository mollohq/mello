#pragma once
#include "capture_source.hpp"

#ifdef _WIN32
#include <dxgi1_2.h>
#include <wrl/client.h>
#include <thread>
#include <atomic>
#include <mutex>

using Microsoft::WRL::ComPtr;

namespace mello::video {

class DxgiCapture : public CaptureSource {
public:
    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override { return width_; }
    uint32_t height() const override { return height_; }
    const char* backend_name() const override { return "DXGI-DDI"; }

    bool get_cursor(CursorData& out) override;

private:
    void capture_thread();

    /// (Re)create the output duplication for `monitor_index_`.
    ///
    /// Duplication is not a durable handle. Windows revokes it on desktop
    /// switches, mode changes, driver resets, and whenever an application takes
    /// exclusive fullscreen — which is precisely when a game stream starts. The
    /// documented recovery is to recreate it, so this is split out and callable
    /// from the capture thread rather than only at init.
    bool recreate_duplication();

    ComPtr<ID3D11Device>           device_;
    ComPtr<ID3D11DeviceContext>    context_;
    ComPtr<IDXGIOutputDuplication> duplication_;

    uint32_t           monitor_index_ = 0;
    uint32_t           width_      = 0;
    uint32_t           height_     = 0;
    uint32_t           target_fps_ = 60;
    std::thread        thread_;
    std::atomic<bool>  running_{false};
    FrameCallback      callback_;

    std::mutex         cursor_mutex_;
    CursorData         cursor_;
    std::vector<uint8_t> cursor_shape_buf_;
};

} // namespace mello::video
#endif
