#ifdef _WIN32
#include "capture_dxgi.hpp"
#include "../util/log.hpp"
#include <dxgi1_2.h>
#include <chrono>
#include <cassert>
#include <thread>

namespace mello::video {

static constexpr const char* TAG = "video/capture";

bool DxgiCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    assert(desc.mode == CaptureMode::Monitor);

    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    monitor_index_ = desc.monitor_index;

    if (!recreate_duplication()) {
        return false;
    }

    MELLO_LOG_INFO(TAG, "Source: Monitor(%u) backend=DXGI-DDI resolution=%ux%u",
        monitor_index_, width_, height_);
    return true;
}

bool DxgiCapture::recreate_duplication() {
    duplication_.Reset();
    const uint32_t previous_w = width_;
    const uint32_t previous_h = height_;

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
    hr = adapter->EnumOutputs(monitor_index_, &output);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "EnumOutputs(%u) failed: hr=0x%08X", monitor_index_, hr);
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

    // A display-mode change also revokes duplication, so a rebuild can come back
    // with different dimensions. The preprocessor and encoder were sized at
    // start_host and cannot be retargeted mid-stream, so feeding them the new
    // size would be worse than stopping. Refuse, loudly — the previous silent
    // death is what this whole change exists to remove.
    if (previous_w != 0 && previous_h != 0 && (width_ != previous_w || height_ != previous_h)) {
        MELLO_LOG_ERROR(TAG,
            "Display %u changed resolution %ux%u -> %ux%u mid-stream; "
            "capture cannot retarget, restart the stream",
            monitor_index_, previous_w, previous_h, width_, height_);
        width_ = previous_w;
        height_ = previous_h;
        return false;
    }

    hr = output1->DuplicateOutput(device_.Get(), &duplication_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "DuplicateOutput failed: hr=0x%08X", hr);
        return false;
    }
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

// Long enough that an idle desktop does not churn duplication, short enough that
// a viewer is not staring at a frozen picture. A rebuild costs one IDR.
static constexpr auto kStallRecoverAfter = std::chrono::seconds(3);
// Brief pause when a rebuild fails, so a mid-transition output is not spun on.
static constexpr auto kAccessLostRetryDelay = std::chrono::milliseconds(250);
// ~10s of retries at the delay above before declaring the output unusable.
static constexpr int kMaxRebuildAttempts = 40;

void DxgiCapture::capture_thread() {
    using clock = std::chrono::steady_clock;

    // Watchdog anchor: last time a frame was actually acquired.
    auto last_frame_tp = clock::now();
    int  rebuild_failures = 0;

    UINT timeout_ms = std::max(1000u / target_fps_ * 2, 34u);

    // Adaptive throttle: accept a frame when elapsed >= target_interval - tolerance.
    // The tolerance is half a vsync period so we snap to the nearest vsync that
    // satisfies target_fps, regardless of monitor refresh rate (60-360Hz).
    // We estimate vsync from the first two acquired frames; until then use 1ms.
    auto target_interval = std::chrono::microseconds(1'000'000 / target_fps_);
    auto tolerance       = std::chrono::microseconds(1000); // updated after first frames
    auto deadline        = target_interval - tolerance;
    auto last_callback   = clock::now() - target_interval;

    clock::time_point first_frame_tp{};
    bool vsync_calibrated = false;

    uint64_t frame_count = 0;
    uint64_t skip_count  = 0;
    auto     stat_start  = clock::now();

    while (running_.load()) {
        // A rebuild can fail (output mid-transition, resolution changed), which
        // leaves duplication_ null. Re-arm here rather than dereferencing it.
        if (!duplication_) {
            if (!recreate_duplication()) {
                // Bounded: a transient transition clears in a few attempts, but a
                // permanent condition (resolution changed mid-stream) must not
                // spin and spam. Give up loudly rather than quietly.
                if (++rebuild_failures > kMaxRebuildAttempts) {
                    MELLO_LOG_ERROR(TAG,
                        "Duplication rebuild failed %d times, giving up on monitor %u",
                        rebuild_failures, monitor_index_);
                    running_ = false;
                    break;
                }
                std::this_thread::sleep_for(kAccessLostRetryDelay);
                continue;
            }
            rebuild_failures = 0;
            MELLO_LOG_INFO(TAG, "Duplication rebuilt, capture resuming");
            last_frame_tp = clock::now();
        }

        ComPtr<IDXGIResource> resource;
        DXGI_OUTDUPL_FRAME_INFO frame_info{};
        HRESULT hr = duplication_->AcquireNextFrame(timeout_ms, &frame_info, &resource);

        if (hr == DXGI_ERROR_WAIT_TIMEOUT) {
            // No desktop update. Normal on a static screen — but also exactly
            // what an exclusive-fullscreen app looks like, because it bypasses
            // the compositor and duplication of that output goes silent with no
            // error at all. Indistinguishable here, so recover on a timer: if
            // nothing arrives for long enough, rebuild the duplication, which is
            // the documented way back after a fullscreen transition.
            if (clock::now() - last_frame_tp >= kStallRecoverAfter) {
                MELLO_LOG_WARN(TAG,
                    "No frame for %llds — rebuilding duplication (fullscreen transition?)",
                    static_cast<long long>(
                        std::chrono::duration_cast<std::chrono::seconds>(kStallRecoverAfter)
                            .count()));
                duplication_.Reset();  // top of loop rebuilds
                last_frame_tp = clock::now();
            }
            continue;
        }

        if (FAILED(hr)) {
            // ACCESS_LOST is expected, not fatal: Windows revokes duplication on
            // desktop switches, mode changes, driver resets and exclusive
            // fullscreen. Dying here left the stream black for the rest of the
            // session with only a warning to show for it.
            if (hr == DXGI_ERROR_ACCESS_LOST || hr == DXGI_ERROR_INVALID_CALL) {
                MELLO_LOG_WARN(TAG, "DXGI access lost (hr=0x%08X), rebuilding duplication", hr);
                duplication_.Reset();  // top of loop rebuilds
                last_frame_tp = clock::now();
                continue;
            }
            MELLO_LOG_ERROR(TAG, "AcquireNextFrame failed: hr=0x%08X", hr);
            running_ = false;
            break;
        }
        last_frame_tp = clock::now();

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

        // Skip cursor-only updates (no new pixel data)
        if (frame_info.LastPresentTime.QuadPart == 0) {
            duplication_->ReleaseFrame();
            continue;
        }

        auto now_tp = clock::now();

        // Calibrate vsync interval from the first two delivered frames
        if (!vsync_calibrated) {
            if (first_frame_tp == clock::time_point{}) {
                first_frame_tp = now_tp;
            } else {
                auto vsync_us = std::chrono::duration_cast<std::chrono::microseconds>(
                    now_tp - first_frame_tp);
                if (vsync_us.count() > 0 && vsync_us.count() < 100'000) {
                    tolerance = vsync_us / 2;
                    deadline  = target_interval - tolerance;
                    if (deadline < std::chrono::microseconds(0))
                        deadline = std::chrono::microseconds(0);
                    vsync_calibrated = true;
                    MELLO_LOG_INFO(TAG, "DXGI-DDI vsync=%.2fms tolerance=%.2fms deadline=%.2fms",
                        vsync_us.count() / 1000.0,
                        tolerance.count() / 1000.0,
                        deadline.count() / 1000.0);
                }
            }
        }

        if (now_tp - last_callback < deadline) {
            duplication_->ReleaseFrame();
            ++skip_count;
            continue;
        }

        ComPtr<ID3D11Texture2D> texture;
        hr = resource.As(&texture);
        if (SUCCEEDED(hr) && callback_) {
            auto now = std::chrono::duration_cast<std::chrono::microseconds>(
                now_tp.time_since_epoch()).count();
            callback_(texture.Get(), static_cast<uint64_t>(now));
            last_callback = now_tp;
            ++frame_count;
        }

        duplication_->ReleaseFrame();

        // Periodic capture-rate diagnostic
        auto stat_elapsed = std::chrono::duration_cast<std::chrono::seconds>(now_tp - stat_start);
        if (stat_elapsed.count() >= 10 && frame_count > 0) {
            double hz = static_cast<double>(frame_count) / stat_elapsed.count();
            MELLO_LOG_INFO(TAG, "DXGI-DDI capture: %.1f delivered / %llu skipped (%llds)",
                hz, (unsigned long long)skip_count, (long long)stat_elapsed.count());
            frame_count = 0;
            skip_count  = 0;
            stat_start  = now_tp;
        }
    }
}

} // namespace mello::video
#endif
