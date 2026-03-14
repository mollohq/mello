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

#ifdef _WIN32
    ::ID3D11Device* d3d11() const;
#endif
};

GraphicsDevice create_d3d11_device();

} // namespace mello::video
