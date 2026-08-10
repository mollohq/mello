#pragma once
#include <cstdint>

#ifdef _WIN32
struct ID3D11Device;
#endif

namespace mello::video {

enum class GraphicsBackend {
    D3D11,
    Metal,
};

struct GraphicsDevice {
    GraphicsBackend backend;
    void* handle;

    // Adapter description, UTF-8, empty when device creation failed. Carried on
    // the device rather than re-queried because remote diagnostics need it: a
    // host's GPU model is the difference between "known-weak encoder backend"
    // and "unexplained stall", and we cannot ask a user to read it out.
    char adapter_name[128];

#ifdef _WIN32
    ::ID3D11Device* d3d11() const;
#endif

#ifdef __APPLE__
    void* metal() const;  // Returns id<MTLDevice> as void*
#endif
};

#ifdef _WIN32
GraphicsDevice create_d3d11_device();
#endif

#ifdef __APPLE__
GraphicsDevice create_metal_device();
#endif

} // namespace mello::video
