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

    // LUID of that adapter, as (HighPart << 32) | LowPart. Zero when device
    // creation failed. A shared texture handle can only be opened by a device
    // on the same adapter, so any other component that must open our textures
    // has to create its device here and not on the system default adapter. On
    // a laptop with two GPUs those are different adapters, and the mismatch
    // shows up as OpenSharedResource1 returning E_INVALIDARG for every frame.
    uint64_t adapter_luid;

#ifdef _WIN32
    ::ID3D11Device* d3d11() const;
#endif

#ifdef __APPLE__
    void* metal() const;  // Returns id<MTLDevice> as void*
#endif
};

#ifdef _WIN32
GraphicsDevice create_d3d11_device();

// LUID of the adapter create_d3d11_device() will take, without creating a
// device. Zero when no adapter is usable. Callers that must open our shared
// textures need this before any pipeline exists.
uint64_t video_adapter_luid();
#endif

#ifdef __APPLE__
GraphicsDevice create_metal_device();
#endif

} // namespace mello::video
