#ifdef _WIN32
#include "capture_dxgi.hpp"
#include "../util/log.hpp"
#include "frame_cadence.hpp"
#include "cursor.hpp"
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
        monitor_index_, width_.load(), height_.load());
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
    const uint32_t new_w = static_cast<uint32_t>(output_desc.DesktopCoordinates.right -
                                                 output_desc.DesktopCoordinates.left);
    const uint32_t new_h = static_cast<uint32_t>(output_desc.DesktopCoordinates.bottom -
                                                 output_desc.DesktopCoordinates.top);

    ComPtr<IDXGIOutput1> output1;
    hr = output.As(&output1);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "QueryInterface IDXGIOutput1 failed: hr=0x%08X", hr);
        return false;
    }

    // A display mode change also revokes duplication, and the rebuild comes
    // back at the new size. The preprocessor scales the whole new desktop into
    // the encode size, which stays fixed for the stream. Refusing the new size
    // here gave up on the stream after about 10 s of failed rebuilds.
    if (previous_w != 0 && previous_h != 0 && (new_w != previous_w || new_h != previous_h)) {
        MELLO_LOG_INFO(TAG, "Display %u changed resolution %ux%u -> %ux%u; capturing the new size",
                       monitor_index_, previous_w, previous_h, new_w, new_h);
    }
    width_.store(new_w, std::memory_order_relaxed);
    height_.store(new_h, std::memory_order_relaxed);

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
    exited_ = std::promise<void>();
    exited_future_ = exited_.get_future();
    thread_ = std::thread(&DxgiCapture::capture_thread, this);
    return true;
}

void DxgiCapture::stop() {
    running_ = false;
    if (!thread_.joinable()) return;

    // Wait with a deadline. AcquireNextFrame can sit in the display driver
    // holding a sync object for as long as an exclusive-fullscreen application
    // owns the output, and it ignores its own timeout there. Joining without a
    // deadline froze the whole client on 2026-09-15.
    if (exited_future_.valid() &&
        exited_future_.wait_for(kStopDeadline) == std::future_status::ready) {
        thread_.join();
        return;
    }

    detached_ = true;
    thread_.detach();
    MELLO_LOG_ERROR(TAG,
        "Capture thread for monitor %u did not stop within %lld ms (stuck in the display "
        "driver). Abandoning it; this capture object must not be destroyed.",
        monitor_index_, static_cast<long long>(kStopDeadline.count()));
}

bool DxgiCapture::get_cursor(CursorData& out) {
    std::lock_guard<std::mutex> lock(cursor_mutex_);
    out.x = cursor_.x;
    out.y = cursor_.y;
    out.visible = cursor_.visible;
    out.shape_changed = cursor_.shape_changed;
    if (out.shape_serial != cursor_.shape_serial) {
        out.shape_serial = cursor_.shape_serial;
        out.shape_w = cursor_.shape_w;
        out.shape_h = cursor_.shape_h;
        out.shape_bgra = cursor_.shape_bgra;
    }
    return true;
}

// Brief pause when a rebuild fails, so a mid-transition output is not spun on.
static constexpr auto kAccessLostRetryDelay = std::chrono::milliseconds(250);
// ~10s of retries at the delay above before declaring the output unusable.
static constexpr int kMaxRebuildAttempts = 40;

void DxgiCapture::capture_thread() {
    using clock = std::chrono::steady_clock;
    // Signals stop() that the thread is really gone, whatever path it leaves by.
    struct ExitSignal {
        std::promise<void>& p;
        ~ExitSignal() {
            try {
                p.set_value();
            } catch (...) {
            }
        }
    } exit_signal{exited_};

    int  rebuild_failures = 0;

    UINT timeout_ms = std::max(1000u / target_fps_ * 2, 34u);

    // Throttle to target_fps with the same rule as the pipeline decimator
    // (frame_cadence.hpp). The old rule reset to the arrival time with a
    // half-vsync tolerance, which quantises the output onto the refresh grid.
    uint64_t cadence_deadline_us = 0;

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
                    failed_ = true;
                    running_ = false;
                    break;
                }
                std::this_thread::sleep_for(kAccessLostRetryDelay);
                continue;
            }
            rebuild_failures = 0;
            MELLO_LOG_INFO(TAG, "Duplication rebuilt, capture resuming");
        }

        ComPtr<IDXGIResource> resource;
        DXGI_OUTDUPL_FRAME_INFO frame_info{};
        HRESULT hr = duplication_->AcquireNextFrame(timeout_ms, &frame_info, &resource);

        if (hr == DXGI_ERROR_WAIT_TIMEOUT) {
            // No desktop update. Normal on a static screen, so silence is never
            // acted on here. The old rule rebuilt duplication after 3 s of
            // silence, which churned on every idle desktop. An exclusive-
            // fullscreen transition is detected by ProcessCapture instead
            // (SHQueryUserNotificationState), which rebuilds on that evidence.
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
                continue;
            }
            MELLO_LOG_ERROR(TAG, "AcquireNextFrame failed: hr=0x%08X", hr);
            failed_ = true;
            running_ = false;
            break;
        }

        // Extract cursor info before releasing the frame. Duplication gives
        // the pointer apart from the desktop image, which has none; the
        // preprocessor draws it into the frame (CursorMode::Composite).
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

                // A monochrome shape holds two masks, so its height is twice
                // the pointer's.
                const uint32_t shape_h = shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME
                                             ? shape_info.Height / 2
                                             : shape_info.Height;
                std::vector<uint8_t> bgra;
                if (SUCCEEDED(hr) && shape_info.Width <= 0xFFFF && shape_h <= 0xFFFF &&
                    pointer_shape_to_bgra(static_cast<PointerShapeKind>(shape_info.Type),
                                          shape_info.Width, shape_h, shape_info.Pitch,
                                          cursor_shape_buf_.data(), required, bgra)) {
                    cursor_.shape_changed = true;
                    cursor_.shape_w = static_cast<uint16_t>(shape_info.Width);
                    cursor_.shape_h = static_cast<uint16_t>(shape_h);
                    cursor_.shape_bgra = std::move(bgra);
                    ++cursor_.shape_serial;
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
        const uint64_t now = static_cast<uint64_t>(
            std::chrono::duration_cast<std::chrono::microseconds>(now_tp.time_since_epoch())
                .count());

        if (!frame_cadence_accepts(now, cadence_deadline_us, target_fps_)) {
            duplication_->ReleaseFrame();
            ++skip_count;
            continue;
        }

        ComPtr<ID3D11Texture2D> texture;
        hr = resource.As(&texture);
        if (SUCCEEDED(hr) && delay_hist_) {
            // LastPresentTime is the QPC time of the present that produced
            // this desktop image.
            LARGE_INTEGER qpc{}, freq{};
            if (QueryPerformanceCounter(&qpc) && QueryPerformanceFrequency(&freq) && freq.QuadPart > 0) {
                const double ms = static_cast<double>(qpc.QuadPart - frame_info.LastPresentTime.QuadPart) *
                                  1000.0 / static_cast<double>(freq.QuadPart);
                delay_hist_->record_ms(ms);
            }
        }
        if (SUCCEEDED(hr) && callback_) {
            callback_(texture.Get(), now);
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
