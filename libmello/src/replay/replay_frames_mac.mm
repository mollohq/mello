// The game clip replay on macOS (replay_frames.hpp): ScreenCaptureKit on the
// game window at the clip quality's size, the CVPixelBuffer to VideoToolbox as
// it is (VideoToolbox converts BGRA itself), and the poster from a retained
// buffer.
#include "replay_frames.hpp"
#include "replay_mux.hpp"
#include "../util/log.hpp"
#include "../video/graphics_device.hpp"

#import <CoreFoundation/CoreFoundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreVideo/CoreVideo.h>

#include <condition_variable>
#include <mutex>

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/frames";

void set_error(std::string* error, const char* text) {
    MELLO_LOG_WARN(TAG, "%s", text);
    if (error) *error = text;
}

class MacReplayFrames final : public ReplayFrames {
public:
    ~MacReplayFrames() override { drop_poster(); }

    void* take(const video::CapturedFrame& frame) override {
        // The capture's own buffer, retained: no copy and no GPU work. The
        // capture's pool has room for the frames that wait (queueDepth).
        if (!frame.texture) return nullptr;
        CVPixelBufferRetain(static_cast<CVPixelBufferRef>(frame.texture));
        return frame.texture;
    }
    void release(void* frame) override {
        if (frame) CVPixelBufferRelease(static_cast<CVPixelBufferRef>(frame));
    }
    bool encode(video::Encoder& encoder, void* frame, uint64_t capture_us, video::EncodedPacket& out) override {
        return encoder.encode(frame, capture_us, out);
    }

    void keep_poster(const video::CapturedFrame& frame) override {
        if (!frame.texture) return;
        auto* buffer = static_cast<CVPixelBufferRef>(frame.texture);
        if (CVPixelBufferGetPixelFormatType(buffer) != kCVPixelFormatType_32BGRA) return;
        CVPixelBufferRetain(buffer);
        CVPixelBufferRef old = nullptr;
        {
            std::lock_guard<std::mutex> lock(poster_mutex_);
            old = poster_;
            poster_ = buffer;
        }
        if (old) CVPixelBufferRelease(old);
        poster_cv_.notify_all();
    }

    bool write_poster(const std::filesystem::path& jpeg, std::chrono::milliseconds wait) override {
        CVPixelBufferRef buffer = nullptr;
        {
            std::unique_lock<std::mutex> lock(poster_mutex_);
            if (!poster_cv_.wait_for(lock, wait, [this] { return poster_ != nullptr; })) return false;
            buffer = poster_;
            poster_ = nullptr;
        }
        CVPixelBufferLockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly);
        const auto* base = static_cast<const uint8_t*>(CVPixelBufferGetBaseAddress(buffer));
        const bool ok = base && write_poster_jpeg(base, static_cast<uint32_t>(CVPixelBufferGetWidth(buffer)),
                                                  static_cast<uint32_t>(CVPixelBufferGetHeight(buffer)),
                                                  static_cast<uint32_t>(CVPixelBufferGetBytesPerRow(buffer)), jpeg);
        CVPixelBufferUnlockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly);
        CVPixelBufferRelease(buffer);
        return ok;
    }

    void drop_poster() override {
        CVPixelBufferRef old = nullptr;
        {
            std::lock_guard<std::mutex> lock(poster_mutex_);
            old = poster_;
            poster_ = nullptr;
        }
        if (old) CVPixelBufferRelease(old);
    }

private:
    std::mutex poster_mutex_;
    std::condition_variable poster_cv_;
    CVPixelBufferRef poster_ = nullptr;  // poster_mutex_, retained
};

std::shared_ptr<video::CaptureManager> make_on_new_device(const video::CaptureSourceDesc& desc, std::string* error) {
    video::GraphicsDevice device = video::create_metal_device();
    if (!device.handle) {
        set_error(error, "no graphics device");
        return nullptr;
    }
    auto source = video::create_capture_source(desc);
    if (!source) {
        CFRelease(device.handle);
        set_error(error, "no capture for this target on macOS");
        return nullptr;
    }
    auto manager = video::CaptureManager::create_from(device, std::move(source), desc);
    if (!manager) set_error(error, "the game window cannot be captured");
    return manager;
}

}  // namespace

std::unique_ptr<ReplayFrames> make_replay_frames(const video::CaptureManager&, uint32_t, uint32_t, std::string*) {
    return std::make_unique<MacReplayFrames>();
}

uint32_t window_owner_pid(void* window) {
    const auto id = static_cast<CGWindowID>(reinterpret_cast<uintptr_t>(window));
    if (id == 0) return 0;
    uint32_t pid = 0;
    const void* values[] = {window};
    CFArrayRef ids = CFArrayCreate(kCFAllocatorDefault, values, 1, nullptr);
    CFArrayRef info = ids ? CGWindowListCreateDescriptionFromArray(ids) : nullptr;
    if (info && CFArrayGetCount(info) > 0) {
        auto dict = static_cast<CFDictionaryRef>(CFArrayGetValueAtIndex(info, 0));
        auto number = static_cast<CFNumberRef>(CFDictionaryGetValue(dict, kCGWindowOwnerPID));
        int32_t value = 0;
        if (number && CFNumberGetValue(number, kCFNumberSInt32Type, &value)) pid = static_cast<uint32_t>(value);
    }
    if (info) CFRelease(info);
    if (ids) CFRelease(ids);
    return pid;
}

void* game_main_window(uint32_t pid) {
    // The largest on-screen window of the process at layer 0 (a normal
    // window): a launcher's small window or a tool palette is not the game.
    CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
                                                 kCGNullWindowID);
    if (!list) return nullptr;
    CGWindowID best = 0;
    double best_area = 0;
    for (CFIndex i = 0; i < CFArrayGetCount(list); ++i) {
        auto w = static_cast<CFDictionaryRef>(CFArrayGetValueAtIndex(list, i));
        int32_t owner = 0, layer = 0;
        uint32_t id = 0;
        auto number = [&](CFStringRef key, CFNumberType type, void* out) {
            auto n = static_cast<CFNumberRef>(CFDictionaryGetValue(w, key));
            return n && CFNumberGetValue(n, type, out);
        };
        if (!number(kCGWindowOwnerPID, kCFNumberSInt32Type, &owner) || static_cast<uint32_t>(owner) != pid) continue;
        if (!number(kCGWindowLayer, kCFNumberSInt32Type, &layer) || layer != 0) continue;
        if (!number(kCGWindowNumber, kCFNumberSInt32Type, &id)) continue;
        CGRect bounds{};
        auto dict = static_cast<CFDictionaryRef>(CFDictionaryGetValue(w, kCGWindowBounds));
        if (!dict || !CGRectMakeWithDictionaryRepresentation(dict, &bounds)) continue;
        const double area = bounds.size.width * bounds.size.height;
        if (bounds.size.width >= 160 && bounds.size.height >= 120 && area > best_area) {
            best_area = area;
            best = id;
        }
    }
    CFRelease(list);
    return reinterpret_cast<void*>(static_cast<uintptr_t>(best));
}

std::shared_ptr<video::CaptureManager> make_window_capture(void* window, uint32_t height, std::string* error) {
    video::CaptureSourceDesc desc{};
    desc.mode = video::CaptureMode::Window;
    desc.hwnd = window;
    desc.window_only = true;      // never the display (CLIPS.md 15.3)
    desc.capture_audio = false;   // the game's sound comes from a process tap
    desc.output_height = height;  // ScreenCaptureKit scales to the clip quality
    return make_on_new_device(desc, error);
}

std::shared_ptr<video::CaptureManager> make_source_capture(const video::CaptureSourceDesc& desc,
                                                           std::string* error) {
    return make_on_new_device(desc, error);
}

}  // namespace mello::replay
