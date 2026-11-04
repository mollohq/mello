#pragma once

// The graphics device that a viewer of this platform runs on, and its
// release: D3D11 on Windows, Metal on Apple, the CPU device on Linux. For the
// users of a ViewerPipeline that are not a stream view (the game clip player
// and the poster from a clip).

#include "graphics_device.hpp"

#ifdef __APPLE__
#include <CoreFoundation/CoreFoundation.h>
#endif

namespace mello::video {

inline GraphicsDevice create_viewer_device() {
#ifdef _WIN32
    return create_d3d11_device();
#elif defined(__APPLE__)
    return create_metal_device();
#elif defined(__linux__)
    return create_cpu_device();
#else
    return {};
#endif
}

inline void release_viewer_device(GraphicsDevice& device) {
    if (!device.handle) return;
#ifdef _WIN32
    device.d3d11()->Release();
#elif defined(__APPLE__)
    CFRelease(device.handle);
#endif
    device.handle = nullptr;
}

}  // namespace mello::video
