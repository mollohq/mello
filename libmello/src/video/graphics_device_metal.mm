#include "graphics_device.hpp"
#include "../util/log.hpp"

#ifdef __APPLE__

#import <Metal/Metal.h>

namespace mello::video {

static constexpr const char* TAG = "video/device";

void* GraphicsDevice::metal() const {
    assert(backend == GraphicsBackend::Metal && "GraphicsDevice is not Metal");
    return handle;
}

GraphicsDevice create_metal_device() {
    @autoreleasepool {
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        if (!device) {
            MELLO_LOG_ERROR(TAG, "MTLCreateSystemDefaultDevice() returned nil");
            return {GraphicsBackend::Metal, nullptr};
        }

        MELLO_LOG_INFO(TAG, "Metal device created: name=\"%s\" unified_memory=%s",
            [[device name] UTF8String],
            device.hasUnifiedMemory ? "true" : "false");

        // Retain — caller owns the device, released in ~VideoPipeline
        void* handle = (__bridge_retained void*)device;
        return {GraphicsBackend::Metal, handle};
    }
}

} // namespace mello::video

#endif
