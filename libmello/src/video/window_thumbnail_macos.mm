#include "window_thumbnail.hpp"
#include "../util/log.hpp"

#ifdef __APPLE__

#import <CoreGraphics/CoreGraphics.h>
#include <dlfcn.h>
#include <algorithm>

namespace mello::video {

static constexpr const char* TAG = "video/thumbnail";

// CGWindowListCreateImage is obsoleted in the macOS 15 SDK headers but still
// present in the CoreGraphics runtime.  Load it via dlsym to avoid the
// compile-time error while keeping a synchronous, thread-safe capture path
// that doesn't rely on ScreenCaptureKit proxy objects (which crash under
// concurrent/off-main-thread access).
using CGWindowListCreateImageFn =
    CGImageRef (*)(CGRect, CGWindowListOption, CGWindowID, CGWindowImageOption);

static CGWindowListCreateImageFn cg_screenshot_fn() {
    static CGWindowListCreateImageFn fn =
        reinterpret_cast<CGWindowListCreateImageFn>(
            dlsym(RTLD_DEFAULT, "CGWindowListCreateImage"));
    return fn;
}

int capture_window_thumbnail(
    void* hwnd_raw,
    uint32_t max_width, uint32_t max_height,
    uint8_t* rgba_out, uint32_t* out_width, uint32_t* out_height)
{
    @autoreleasepool {
        CGWindowID target_wid = (CGWindowID)(uintptr_t)hwnd_raw;

        auto fn = cg_screenshot_fn();
        if (!fn) {
            MELLO_LOG_WARN(TAG, "CGWindowListCreateImage unavailable at runtime");
            return -1;
        }

        CGImageRef img = fn(
            CGRectNull,
            kCGWindowListOptionIncludingWindow,
            target_wid,
            kCGWindowImageBoundsIgnoreFraming | kCGWindowImageNominalResolution);

        if (!img) return -1;

        uint32_t img_w = (uint32_t)CGImageGetWidth(img);
        uint32_t img_h = (uint32_t)CGImageGetHeight(img);
        if (img_w == 0 || img_h == 0) {
            CGImageRelease(img);
            return -1;
        }

        float scale = std::min(
            (float)max_width  / img_w,
            (float)max_height / img_h);
        scale = std::min(scale, 1.0f);
        uint32_t thumb_w = std::max(1u, (uint32_t)(img_w * scale));
        uint32_t thumb_h = std::max(1u, (uint32_t)(img_h * scale));

        CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
        CGContextRef ctx = CGBitmapContextCreate(
            rgba_out, thumb_w, thumb_h,
            8, thumb_w * 4,
            cs,
            kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);

        if (!ctx) {
            CGColorSpaceRelease(cs);
            CGImageRelease(img);
            return -1;
        }

        CGContextSetInterpolationQuality(ctx, kCGInterpolationMedium);
        CGContextDrawImage(ctx, CGRectMake(0, 0, thumb_w, thumb_h), img);

        *out_width  = thumb_w;
        *out_height = thumb_h;

        CGContextRelease(ctx);
        CGColorSpaceRelease(cs);
        CGImageRelease(img);
        return 0;
    }
}

} // namespace mello::video

#endif
