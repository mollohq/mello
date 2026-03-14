#ifdef _WIN32
#include "capture_dxgi.hpp"
#include "../util/log.hpp"
#include <dxgi1_2.h>
#include <chrono>
#include <cassert>

namespace mello::video {

static constexpr const char* TAG = "video/capture";

bool DxgiCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    assert(desc.mode == CaptureMode::Monitor);

    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);

    ComPtr<IDXGIDevice> dxgi_device;
    HRESULT hr = device_->QueryInterface(IID_PPV_ARGS(&dxgi_device));
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "QueryInterface IDXGIDevice failed: hr=0x%08X", hr);
        return false;
    }

    ComPtr<IDXGIAdapter> adapter;
    hr = dxgi_device->GetAdapter(&adapter);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "GetAdapter failed: hr=0x%08X", hr);
        return false;
    }

    ComPtr<IDXGIOutput> output;
    hr = adapter->EnumOutputs(desc.monitor_index, &output);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "EnumOutputs(%u) failed: hr=0x%08X", desc.monitor_index, hr);
        return false;
    }

    DXGI_OUTPUT_DESC output_desc{};
    output->GetDesc(&output_desc);
    width_  = output_desc.DesktopCoordinates.right - output_desc.DesktopCoordinates.left;
    height_ = output_desc.DesktopCoordinates.bottom - output_desc.DesktopCoordinates.top;

    ComPtr<IDXGIOutput1> output1;
    hr = output.As(&output1);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "QueryInterface IDXGIOutput1 failed: hr=0x%08X", hr);
        return false;
    }

    hr = output1->DuplicateOutput(device_.Get(), &duplication_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "DuplicateOutput failed: hr=0x%08X", hr);
        return false;
    }

    MELLO_LOG_INFO(TAG, "Source: Monitor(%u) backend=DXGI-DDI resolution=%ux%u",
        desc.monitor_index, width_, height_);
    return true;
}

bool DxgiCapture::start(uint32_t target_fps, FrameCallback callback) {
    if (running_.load()) return false;
    target_fps_ = target_fps;
    callback_ = std::move(callback);
    running_ = true;
    thread_ = std::thread(&DxgiCapture::capture_thread, this);
    return true;
}

void DxgiCapture::stop() {
    running_ = false;
    if (thread_.joinable()) thread_.join();
}

bool DxgiCapture::get_cursor(CursorData& out) {
    std::lock_guard<std::mutex> lock(cursor_mutex_);
    out = cursor_;
    return true;
}

void DxgiCapture::capture_thread() {
    using clock = std::chrono::steady_clock;
    auto frame_interval = std::chrono::microseconds(1'000'000 / target_fps_);

    while (running_.load()) {
        auto frame_start = clock::now();

        ComPtr<IDXGIResource> resource;
        DXGI_OUTDUPL_FRAME_INFO frame_info{};
        HRESULT hr = duplication_->AcquireNextFrame(16, &frame_info, &resource);

        if (hr == DXGI_ERROR_WAIT_TIMEOUT) {
            auto elapsed = clock::now() - frame_start;
            if (elapsed < frame_interval) {
                std::this_thread::sleep_for(frame_interval - elapsed);
            }
            continue;
        }

        if (FAILED(hr)) {
            if (hr == DXGI_ERROR_ACCESS_LOST) {
                MELLO_LOG_WARN(TAG, "DXGI access lost, capture will need re-init");
            } else {
                MELLO_LOG_ERROR(TAG, "AcquireNextFrame failed: hr=0x%08X", hr);
            }
            running_ = false;
            break;
        }

        // Extract cursor info before releasing the frame
        if (frame_info.LastMouseUpdateTime.QuadPart != 0) {
            std::lock_guard<std::mutex> lock(cursor_mutex_);
            cursor_.x = frame_info.PointerPosition.Position.x;
            cursor_.y = frame_info.PointerPosition.Position.y;
            cursor_.visible = frame_info.PointerPosition.Visible != 0;

            if (frame_info.PointerShapeBufferSize > 0) {
                cursor_shape_buf_.resize(frame_info.PointerShapeBufferSize);
                DXGI_OUTDUPL_POINTER_SHAPE_INFO shape_info{};
                UINT required = 0;
                hr = duplication_->GetFramePointerShape(
                    static_cast<UINT>(cursor_shape_buf_.size()),
                    cursor_shape_buf_.data(),
                    &required,
                    &shape_info);

                if (SUCCEEDED(hr) && shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR) {
                    cursor_.shape_changed = true;
                    cursor_.shape_w = static_cast<uint16_t>(shape_info.Width);
                    cursor_.shape_h = static_cast<uint16_t>(shape_info.Height);
                    size_t pixel_bytes = static_cast<size_t>(cursor_.shape_w) * cursor_.shape_h * 4;
                    cursor_.shape_rgba.assign(
                        cursor_shape_buf_.data(),
                        cursor_shape_buf_.data() + std::min(pixel_bytes, cursor_shape_buf_.size()));
                } else {
                    cursor_.shape_changed = false;
                }
            }
        }

        ComPtr<ID3D11Texture2D> texture;
        hr = resource.As(&texture);
        if (SUCCEEDED(hr) && callback_) {
            auto now = std::chrono::duration_cast<std::chrono::microseconds>(
                clock::now().time_since_epoch()).count();
            callback_(texture.Get(), static_cast<uint64_t>(now));
        }

        duplication_->ReleaseFrame();

        auto elapsed = clock::now() - frame_start;
        if (elapsed < frame_interval) {
            std::this_thread::sleep_for(frame_interval - elapsed);
        }
    }
}

} // namespace mello::video
#endif
