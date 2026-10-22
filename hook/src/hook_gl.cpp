#include "hook_gl.hpp"

#include <windows.h>

#include <d3d11.h>
#include <dxgi.h>
#include <GL/gl.h>

#include <atomic>
#include <cstring>

#include <detours/detours.h>

#include "hook_log.hpp"
#include "hook_state.hpp"

namespace mello_hook {
namespace {

// --- The GL entry points. opengl32.dll exports GL 1.1; the rest comes from
// wglGetProcAddress with the game's context current. The hook links no GL
// library, so a game that never loads opengl32.dll never gets it. ---

using SwapBuffersFn = BOOL(WINAPI*)(HDC);
using GetCurrentContextFn = HGLRC(WINAPI*)();
using GetProcAddressFn = PROC(WINAPI*)(LPCSTR);

using GetIntegervFn = void(APIENTRY*)(GLenum, GLint*);
using IsEnabledFn = GLboolean(APIENTRY*)(GLenum);
using CapFn = void(APIENTRY*)(GLenum);
using ReadBufferFn = void(APIENTRY*)(GLenum);
using ReadPixelsFn = void(APIENTRY*)(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, void*);
using GenFn = void(APIENTRY*)(GLsizei, GLuint*);
using DeleteFn = void(APIENTRY*)(GLsizei, const GLuint*);
using FlushFn = void(APIENTRY*)();
using GetErrorFn = GLenum(APIENTRY*)();
using PixelStoreiFn = void(APIENTRY*)(GLenum, GLint);
using BindFn = void(APIENTRY*)(GLenum, GLuint);
using FramebufferTexture2DFn = void(APIENTRY*)(GLenum, GLenum, GLenum, GLuint, GLint);
using CheckFramebufferStatusFn = GLenum(APIENTRY*)(GLenum);
using BlitFramebufferFn = void(APIENTRY*)(GLint, GLint, GLint, GLint, GLint, GLint, GLint, GLint,
                                          GLbitfield, GLenum);
using BufferDataFn = void(APIENTRY*)(GLenum, ptrdiff_t, const void*, GLenum);
using MapBufferFn = void*(APIENTRY*)(GLenum, GLenum);
using UnmapBufferFn = GLboolean(APIENTRY*)(GLenum);
using DxOpenDeviceFn = HANDLE(WINAPI*)(void*);
using DxCloseDeviceFn = BOOL(WINAPI*)(HANDLE);
using DxRegisterObjectFn = HANDLE(WINAPI*)(HANDLE, void*, GLuint, GLenum, GLenum);
using DxUnregisterObjectFn = BOOL(WINAPI*)(HANDLE, HANDLE);
using DxLockFn = BOOL(WINAPI*)(HANDLE, GLint, HANDLE*);

constexpr GLenum kReadFramebuffer = 0x8CA8;
constexpr GLenum kDrawFramebuffer = 0x8CA9;
constexpr GLenum kReadFramebufferBinding = 0x8CAA;
constexpr GLenum kDrawFramebufferBinding = 0x8CA6;
constexpr GLenum kColorAttachment0 = 0x8CE0;
constexpr GLenum kFramebufferComplete = 0x8CD5;
constexpr GLenum kFramebufferSrgb = 0x8DB9;
constexpr GLenum kPixelPackBuffer = 0x88EB;
constexpr GLenum kPixelPackBufferBinding = 0x88ED;
constexpr GLenum kStreamRead = 0x88E1;
constexpr GLenum kReadOnly = 0x88B8;
constexpr GLenum kBgra = 0x80E1;
constexpr GLenum kAccessWriteDiscard = 0x0002;  // WGL_ACCESS_WRITE_DISCARD_NV

struct GlApi {
    GetIntegervFn            GetIntegerv = nullptr;
    IsEnabledFn              IsEnabled = nullptr;
    CapFn                    Enable = nullptr;
    CapFn                    Disable = nullptr;
    ReadBufferFn             ReadBuffer = nullptr;
    ReadPixelsFn             ReadPixels = nullptr;
    GenFn                    GenTextures = nullptr;
    DeleteFn                 DeleteTextures = nullptr;
    FlushFn                  Flush = nullptr;
    GetErrorFn               GetError = nullptr;
    PixelStoreiFn            PixelStorei = nullptr;
    GenFn                    GenFramebuffers = nullptr;
    DeleteFn                 DeleteFramebuffers = nullptr;
    BindFn                   BindFramebuffer = nullptr;
    FramebufferTexture2DFn   FramebufferTexture2D = nullptr;
    CheckFramebufferStatusFn CheckFramebufferStatus = nullptr;
    BlitFramebufferFn        BlitFramebuffer = nullptr;
    GenFn                    GenBuffers = nullptr;
    DeleteFn                 DeleteBuffers = nullptr;
    BindFn                   BindBuffer = nullptr;
    BufferDataFn             BufferData = nullptr;
    MapBufferFn              MapBuffer = nullptr;
    UnmapBufferFn            UnmapBuffer = nullptr;
    DxOpenDeviceFn           DXOpenDevice = nullptr;
    DxCloseDeviceFn          DXCloseDevice = nullptr;
    DxRegisterObjectFn       DXRegisterObject = nullptr;
    DxUnregisterObjectFn     DXUnregisterObject = nullptr;
    DxLockFn                 DXLockObjects = nullptr;
    DxLockFn                 DXUnlockObjects = nullptr;
};

SwapBuffersFn       g_real_swap_buffers = nullptr;
GetCurrentContextFn g_get_current_context = nullptr;
GetProcAddressFn    g_get_proc_address = nullptr;
HMODULE             g_opengl32 = nullptr;
GlApi               g_gl;

std::atomic<bool> g_disabled{false};
std::atomic<bool> g_in_capture{false};

template <typename Fn>
void load_export(const char* name, Fn* out) {
    *out = reinterpret_cast<Fn>(GetProcAddress(g_opengl32, name));
}

template <typename Fn>
void load_ext(const char* name, Fn* out) {
    PROC p = g_get_proc_address ? g_get_proc_address(name) : nullptr;
    // Some drivers answer 1, 2, 3 or -1 for a name they do not have.
    const uintptr_t v = reinterpret_cast<uintptr_t>(p);
    *out = (v <= 3 || v == static_cast<uintptr_t>(-1)) ? nullptr : reinterpret_cast<Fn>(p);
}

// Loads every entry point with the game's context current. Build path only.
void load_gl() {
    load_export("glGetIntegerv", &g_gl.GetIntegerv);
    load_export("glIsEnabled", &g_gl.IsEnabled);
    load_export("glEnable", &g_gl.Enable);
    load_export("glDisable", &g_gl.Disable);
    load_export("glReadBuffer", &g_gl.ReadBuffer);
    load_export("glReadPixels", &g_gl.ReadPixels);
    load_export("glGenTextures", &g_gl.GenTextures);
    load_export("glDeleteTextures", &g_gl.DeleteTextures);
    load_export("glFlush", &g_gl.Flush);
    load_export("glGetError", &g_gl.GetError);
    load_export("glPixelStorei", &g_gl.PixelStorei);
    load_ext("glGenFramebuffers", &g_gl.GenFramebuffers);
    load_ext("glDeleteFramebuffers", &g_gl.DeleteFramebuffers);
    load_ext("glBindFramebuffer", &g_gl.BindFramebuffer);
    load_ext("glFramebufferTexture2D", &g_gl.FramebufferTexture2D);
    load_ext("glCheckFramebufferStatus", &g_gl.CheckFramebufferStatus);
    load_ext("glBlitFramebuffer", &g_gl.BlitFramebuffer);
    load_ext("glGenBuffers", &g_gl.GenBuffers);
    load_ext("glDeleteBuffers", &g_gl.DeleteBuffers);
    load_ext("glBindBuffer", &g_gl.BindBuffer);
    load_ext("glBufferData", &g_gl.BufferData);
    load_ext("glMapBuffer", &g_gl.MapBuffer);
    load_ext("glUnmapBuffer", &g_gl.UnmapBuffer);
    load_ext("wglDXOpenDeviceNV", &g_gl.DXOpenDevice);
    load_ext("wglDXCloseDeviceNV", &g_gl.DXCloseDevice);
    load_ext("wglDXRegisterObjectNV", &g_gl.DXRegisterObject);
    load_ext("wglDXUnregisterObjectNV", &g_gl.DXUnregisterObject);
    load_ext("wglDXLockObjectsNV", &g_gl.DXLockObjects);
    load_ext("wglDXUnlockObjectsNV", &g_gl.DXUnlockObjects);
}

bool have_core() {
    return g_gl.GetIntegerv && g_gl.IsEnabled && g_gl.Enable && g_gl.Disable && g_gl.ReadBuffer &&
           g_gl.ReadPixels && g_gl.GenTextures && g_gl.DeleteTextures && g_gl.Flush &&
           g_gl.GetError && g_gl.PixelStorei && g_gl.BindFramebuffer;
}

bool have_interop() {
    return g_gl.GenFramebuffers && g_gl.DeleteFramebuffers && g_gl.FramebufferTexture2D &&
           g_gl.CheckFramebufferStatus && g_gl.BlitFramebuffer && g_gl.DXOpenDevice &&
           g_gl.DXCloseDevice && g_gl.DXRegisterObject && g_gl.DXUnregisterObject &&
           g_gl.DXLockObjects && g_gl.DXUnlockObjects;
}

bool have_pbo() {
    return g_gl.GenBuffers && g_gl.DeleteBuffers && g_gl.BindBuffer && g_gl.BufferData &&
           g_gl.MapBuffer && g_gl.UnmapBuffer;
}

// --- Capture resources. Present thread only; raw pointers for the
// structured exception guard. ---

struct Resources {
    HGLRC  context = nullptr;      // GL names below belong to this context
    UINT   width = 0;
    UINT   height = 0;
    bool   ready = false;
    bool   interop = false;
    uint32_t next = 0;
    // Interop path.
    ID3D11Device*    device = nullptr;
    ID3D11Texture2D* texture[MELLO_HOOK_TEXTURE_COUNT]{};
    HANDLE           dx_device = nullptr;
    HANDLE           dx_object[MELLO_HOOK_TEXTURE_COUNT]{};
    GLuint           gl_texture[MELLO_HOOK_TEXTURE_COUNT]{};
    GLuint           fbo[MELLO_HOOK_TEXTURE_COUNT]{};
    // Memory path.
    GLuint   pbo[MELLO_HOOK_TEXTURE_COUNT]{};
    bool     pending[MELLO_HOOK_TEXTURE_COUNT]{};
    uint64_t pending_qpc[MELLO_HOOK_TEXTURE_COUNT]{};
    HANDLE   mapping = nullptr;
    uint8_t* frames = nullptr;
    uint32_t pitch = 0;
    uint32_t frame_bytes = 0;
    // Time the copies took on the game's thread, for the log at release.
    uint64_t copy_ticks = 0;
    uint64_t copies = 0;
};

Resources g_res;

// Releases everything. GL names are deleted only when their context is
// current on this thread; else they are left to the context's own end.
void release_resources() {
    if (g_res.copies > 0) {
        LARGE_INTEGER freq{};
        QueryPerformanceFrequency(&freq);
        log_line("GL: %llu copies through %s, %.1f us each on the game's thread",
                 static_cast<unsigned long long>(g_res.copies),
                 g_res.interop ? "the interop" : "memory",
                 static_cast<double>(g_res.copy_ticks) * 1e6 / static_cast<double>(freq.QuadPart) /
                     static_cast<double>(g_res.copies));
    }
    const bool current = g_get_current_context && g_res.context &&
                         g_get_current_context() == g_res.context;
    for (uint32_t i = 0; i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
        if (current && g_res.dx_object[i] && g_gl.DXUnregisterObject) {
            g_gl.DXUnregisterObject(g_res.dx_device, g_res.dx_object[i]);
        }
        if (g_res.texture[i]) g_res.texture[i]->Release();
    }
    if (current) {
        if (g_res.fbo[0] && g_gl.DeleteFramebuffers) g_gl.DeleteFramebuffers(2, g_res.fbo);
        if (g_res.gl_texture[0] && g_gl.DeleteTextures) g_gl.DeleteTextures(2, g_res.gl_texture);
        if (g_res.pbo[0] && g_gl.DeleteBuffers) g_gl.DeleteBuffers(2, g_res.pbo);
        if (g_res.dx_device && g_gl.DXCloseDevice) g_gl.DXCloseDevice(g_res.dx_device);
        if (g_gl.GetError) {
            while (g_gl.GetError() != GL_NO_ERROR) {
            }
        }
    }
    if (g_res.device) g_res.device->Release();
    if (g_res.frames) UnmapViewOfFile(g_res.frames);
    if (g_res.mapping) CloseHandle(g_res.mapping);
    g_res = Resources{};
}

// The game's GL state that a blit or a read changes. Saved before, put back
// after, every time.
struct SavedState {
    GLint read_fbo = 0;
    GLint draw_fbo = 0;
    GLint read_buffer = 0;
    GLint pack_buffer = 0;
    GLint pack_alignment = 4;
    GLint pack_row_length = 0;
    GLint pack_skip_rows = 0;
    GLint pack_skip_pixels = 0;
    GLboolean scissor = GL_FALSE;
    GLboolean srgb = GL_FALSE;
};

void save_state(SavedState* s) {
    g_gl.GetIntegerv(kReadFramebufferBinding, &s->read_fbo);
    g_gl.GetIntegerv(kDrawFramebufferBinding, &s->draw_fbo);
    s->scissor = g_gl.IsEnabled(GL_SCISSOR_TEST);
    s->srgb = g_gl.IsEnabled(kFramebufferSrgb);
    if (g_gl.BindBuffer) g_gl.GetIntegerv(kPixelPackBufferBinding, &s->pack_buffer);
    g_gl.GetIntegerv(GL_PACK_ALIGNMENT, &s->pack_alignment);
    g_gl.GetIntegerv(GL_PACK_ROW_LENGTH, &s->pack_row_length);
    g_gl.GetIntegerv(GL_PACK_SKIP_ROWS, &s->pack_skip_rows);
    g_gl.GetIntegerv(GL_PACK_SKIP_PIXELS, &s->pack_skip_pixels);
    // The read buffer belongs to the framebuffer bound for reading: read it
    // for the default framebuffer, which is the one this code changes.
    g_gl.BindFramebuffer(kReadFramebuffer, 0);
    g_gl.GetIntegerv(GL_READ_BUFFER, &s->read_buffer);
    if (s->scissor) g_gl.Disable(GL_SCISSOR_TEST);
    if (s->srgb) g_gl.Disable(kFramebufferSrgb);
}

void restore_state(const SavedState& s) {
    g_gl.BindFramebuffer(kReadFramebuffer, 0);
    g_gl.ReadBuffer(static_cast<GLenum>(s.read_buffer));
    g_gl.BindFramebuffer(kReadFramebuffer, static_cast<GLuint>(s.read_fbo));
    g_gl.BindFramebuffer(kDrawFramebuffer, static_cast<GLuint>(s.draw_fbo));
    if (g_gl.BindBuffer) g_gl.BindBuffer(kPixelPackBuffer, static_cast<GLuint>(s.pack_buffer));
    g_gl.PixelStorei(GL_PACK_ALIGNMENT, s.pack_alignment);
    g_gl.PixelStorei(GL_PACK_ROW_LENGTH, s.pack_row_length);
    g_gl.PixelStorei(GL_PACK_SKIP_ROWS, s.pack_skip_rows);
    g_gl.PixelStorei(GL_PACK_SKIP_PIXELS, s.pack_skip_pixels);
    if (s.scissor) g_gl.Enable(GL_SCISSOR_TEST);
    if (s.srgb) g_gl.Enable(kFramebufferSrgb);
    // Leave no error of ours for the game's glGetError.
    while (g_gl.GetError() != GL_NO_ERROR) {
    }
}

// The adapter the client opens shared textures on, else the default one.
IDXGIAdapter* client_adapter(uint64_t luid) {
    IDXGIFactory1* factory = nullptr;
    if (luid == 0 || FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1),
                                               reinterpret_cast<void**>(&factory)))) {
        return nullptr;
    }
    IDXGIAdapter* found = nullptr;
    IDXGIAdapter* adapter = nullptr;
    for (UINT i = 0; factory->EnumAdapters(i, &adapter) == S_OK; ++i) {
        DXGI_ADAPTER_DESC desc{};
        const bool match =
            SUCCEEDED(adapter->GetDesc(&desc)) &&
            ((static_cast<uint64_t>(static_cast<uint32_t>(desc.AdapterLuid.HighPart)) << 32) |
             static_cast<uint32_t>(desc.AdapterLuid.LowPart)) == luid;
        if (match) {
            found = adapter;
            break;
        }
        adapter->Release();
    }
    factory->Release();
    return found;
}

// The interop path: a D3D11 device in the game on the client's adapter, two
// legacy-shared textures, registered with GL and attached to framebuffers.
bool build_interop(UINT width, UINT height, uint64_t client_luid) {
    if (!have_interop()) return false;

    IDXGIAdapter* adapter = client_adapter(client_luid);
    const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
                                        D3D_FEATURE_LEVEL_10_0};
    const HRESULT made = D3D11CreateDevice(
        adapter, adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, levels,
        ARRAYSIZE(levels), D3D11_SDK_VERSION, &g_res.device, nullptr, nullptr);
    if (adapter) adapter->Release();
    if (FAILED(made) || !g_res.device) return false;

    g_res.dx_device = g_gl.DXOpenDevice(g_res.device);
    if (!g_res.dx_device) {
        log_line("GL: wglDXOpenDeviceNV refused the D3D11 device (another GPU?)");
        return false;
    }

    D3D11_TEXTURE2D_DESC td{};
    td.Width = width;
    td.Height = height;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
    td.MiscFlags = D3D11_RESOURCE_MISC_SHARED;

    uint32_t handles[MELLO_HOOK_TEXTURE_COUNT]{};
    g_gl.GenTextures(2, g_res.gl_texture);
    g_gl.GenFramebuffers(2, g_res.fbo);
    GLint draw_fbo = 0;
    g_gl.GetIntegerv(kDrawFramebufferBinding, &draw_fbo);
    bool ok = true;
    for (uint32_t i = 0; ok && i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
        IDXGIResource* resource = nullptr;
        HANDLE shared = nullptr;
        ok = SUCCEEDED(g_res.device->CreateTexture2D(&td, nullptr, &g_res.texture[i])) &&
             SUCCEEDED(g_res.texture[i]->QueryInterface(__uuidof(IDXGIResource),
                                                        reinterpret_cast<void**>(&resource))) &&
             SUCCEEDED(resource->GetSharedHandle(&shared)) && shared;
        if (resource) resource->Release();
        if (!ok) break;
        handles[i] = static_cast<uint32_t>(reinterpret_cast<uintptr_t>(shared));
        g_res.dx_object[i] = g_gl.DXRegisterObject(g_res.dx_device, g_res.texture[i],
                                                   g_res.gl_texture[i], GL_TEXTURE_2D,
                                                   kAccessWriteDiscard);
        if (!g_res.dx_object[i] || !g_gl.DXLockObjects(g_res.dx_device, 1, &g_res.dx_object[i])) {
            ok = false;
            break;
        }
        g_gl.BindFramebuffer(kDrawFramebuffer, g_res.fbo[i]);
        g_gl.FramebufferTexture2D(kDrawFramebuffer, kColorAttachment0, GL_TEXTURE_2D,
                                  g_res.gl_texture[i], 0);
        ok = g_gl.CheckFramebufferStatus(kDrawFramebuffer) == kFramebufferComplete;
        g_gl.DXUnlockObjects(g_res.dx_device, 1, &g_res.dx_object[i]);
    }
    g_gl.BindFramebuffer(kDrawFramebuffer, static_cast<GLuint>(draw_fbo));
    while (g_gl.GetError() != GL_NO_ERROR) {
    }
    if (!ok) {
        log_line("GL: the interop textures failed; frames go through memory");
        return false;
    }

    g_res.interop = true;
    HookState::instance().publish_description(MELLO_HOOK_API_OPENGL, width, height,
                                              DXGI_FORMAT_B8G8R8A8_UNORM, client_luid, handles, 0);
    log_line("GL: capturing %ux%u through WGL_NV_DX_interop2, handles=%08x,%08x", width, height,
             handles[0], handles[1]);
    return true;
}

// The memory path: two pixel buffer objects and the frame block.
bool build_memory(UINT width, UINT height) {
    if (!have_pbo()) {
        log_line("GL: no pixel buffer objects (GL 2.1); no capture");
        return false;
    }
    g_res.pitch = width * 4;
    g_res.frame_bytes = g_res.pitch * height;
    g_gl.GenBuffers(2, g_res.pbo);
    GLint pack = 0;
    g_gl.GetIntegerv(kPixelPackBufferBinding, &pack);
    for (uint32_t i = 0; i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
        g_gl.BindBuffer(kPixelPackBuffer, g_res.pbo[i]);
        g_gl.BufferData(kPixelPackBuffer, static_cast<ptrdiff_t>(g_res.frame_bytes), nullptr,
                        kStreamRead);
    }
    g_gl.BindBuffer(kPixelPackBuffer, static_cast<GLuint>(pack));
    while (g_gl.GetError() != GL_NO_ERROR) {
    }

    char name[64];
    object_name(name, sizeof(name), MELLO_HOOK_NAME_FRAMES, GetCurrentProcessId());
    const uint64_t total = static_cast<uint64_t>(g_res.frame_bytes) * MELLO_HOOK_TEXTURE_COUNT;
    g_res.mapping = CreateFileMappingA(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE,
                                       static_cast<DWORD>(total >> 32),
                                       static_cast<DWORD>(total & 0xFFFFFFFF), name);
    if (g_res.mapping) {
        g_res.frames = static_cast<uint8_t*>(
            MapViewOfFile(g_res.mapping, FILE_MAP_ALL_ACCESS, 0, 0, static_cast<SIZE_T>(total)));
    }
    if (!g_res.frames) {
        log_line("GL: frame block of %llu bytes failed: %lu", total, GetLastError());
        return false;
    }
    if (MelloHookInfo* info = HookState::instance().info()) {
        info->cpu_frame_bytes = g_res.frame_bytes;
        info->cpu_pitch = g_res.pitch;
    }
    const uint32_t handles[MELLO_HOOK_TEXTURE_COUNT] = {0, 0};
    HookState::instance().publish_description(MELLO_HOOK_API_OPENGL, width, height,
                                              DXGI_FORMAT_B8G8R8A8_UNORM, 0, handles,
                                              MELLO_HOOK_FLAG_CPU_COPY);
    log_line("GL: capturing %ux%u through memory (pixel buffer objects), %u bytes a frame", width,
             height, g_res.frame_bytes);
    return true;
}

bool build_resources(HGLRC context, UINT width, UINT height) {
    release_resources();
    load_gl();
    HookState& state = HookState::instance();
    if (!have_core()) {
        log_line("GL: the context has no framebuffer objects (GL 3.0); no capture");
        state.set_error(MELLO_HOOK_ERR_NO_DEVICE);
        return false;
    }
    g_res.context = context;
    g_res.width = width;
    g_res.height = height;

    MelloHookInfo* info = state.info();
    const uint64_t client_luid = info ? info->client_adapter_luid : 0;
    const bool readback_asked =
        info && (info->client_options & MELLO_HOOK_OPTION_GL_READBACK) != 0;
    if (!readback_asked && build_interop(width, height, client_luid)) {
        g_res.ready = true;
    } else {
        // The interop half-built: drop it, keep the context and size.
        for (uint32_t i = 0; i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
            if (g_res.dx_object[i]) g_gl.DXUnregisterObject(g_res.dx_device, g_res.dx_object[i]);
            g_res.dx_object[i] = nullptr;
            if (g_res.texture[i]) g_res.texture[i]->Release();
            g_res.texture[i] = nullptr;
        }
        if (g_res.fbo[0]) g_gl.DeleteFramebuffers(2, g_res.fbo);
        if (g_res.gl_texture[0]) g_gl.DeleteTextures(2, g_res.gl_texture);
        if (g_res.dx_device) g_gl.DXCloseDevice(g_res.dx_device);
        if (g_res.device) g_res.device->Release();
        g_res.dx_device = nullptr;
        g_res.device = nullptr;
        g_res.fbo[0] = g_res.fbo[1] = 0;
        g_res.gl_texture[0] = g_res.gl_texture[1] = 0;
        g_res.interop = false;
        if (!build_memory(width, height)) {
            release_resources();
            state.set_error(MELLO_HOOK_ERR_SHARED_TEXTURE);
            return false;
        }
        g_res.ready = true;
    }
    state.set_error(MELLO_HOOK_OK);
    return true;
}

// Copies the bottom-up rows of a GL read into the top-down frame slot.
void copy_flipped(const uint8_t* source, uint32_t slot) {
    uint8_t* target = g_res.frames + static_cast<size_t>(slot) * g_res.frame_bytes;
    for (UINT row = 0; row < g_res.height; ++row) {
        std::memcpy(target + static_cast<size_t>(row) * g_res.pitch,
                    source + static_cast<size_t>(g_res.height - 1 - row) * g_res.pitch, g_res.pitch);
    }
}

// The body of the SwapBuffers detour. The game's context is current.
void capture_swap(HDC dc) {
    HookState& state = HookState::instance();
    if (!state.capture_wanted()) {
        if (g_res.ready) {
            log_line("GL: client stopped asking for frames; releasing capture resources");
            release_resources();
        }
        return;
    }
    const HGLRC context = g_get_current_context ? g_get_current_context() : nullptr;
    if (!context) return;
    RECT client{};
    const HWND window = WindowFromDC(dc);
    if (!window || !GetClientRect(window, &client)) return;
    const UINT width = static_cast<UINT>(client.right - client.left);
    const UINT height = static_cast<UINT>(client.bottom - client.top);
    if (width == 0 || height == 0) return;

    if (!g_res.ready || g_res.context != context || g_res.width != width ||
        g_res.height != height) {
        if (!build_resources(context, width, height)) return;
    }
    if (!state.copy_due(qpc_now())) return;

    const int64_t copy_started = qpc_now();
    SavedState saved;
    save_state(&saved);
    const uint32_t slot = g_res.next;
    const GLint w = static_cast<GLint>(width);
    const GLint h = static_cast<GLint>(height);
    g_gl.ReadBuffer(GL_BACK);

    if (g_res.interop) {
        if (!g_gl.DXLockObjects(g_res.dx_device, 1, &g_res.dx_object[slot])) {
            restore_state(saved);
            state.count_drop();
            return;
        }
        g_gl.BindFramebuffer(kDrawFramebuffer, g_res.fbo[slot]);
        // Flipped: GL rows go bottom up, the client's go top down.
        g_gl.BlitFramebuffer(0, 0, w, h, 0, h, w, 0, GL_COLOR_BUFFER_BIT, GL_NEAREST);
        g_gl.DXUnlockObjects(g_res.dx_device, 1, &g_res.dx_object[slot]);
        // Submit the blit now, so it does not wait for the game's next batch.
        g_gl.Flush();
        restore_state(saved);
        g_res.copy_ticks += static_cast<uint64_t>(qpc_now() - copy_started);
        ++g_res.copies;
        g_res.next = (slot + 1) % MELLO_HOOK_TEXTURE_COUNT;
        state.count_copy();
        state.publish_frame(slot, static_cast<uint64_t>(qpc_now()));
        state.signal_frame();
        return;
    }

    // Memory: start this frame's read into its buffer object (the GPU does
    // it later), then map the one started a due frame ago, which is done.
    g_gl.BindBuffer(kPixelPackBuffer, g_res.pbo[slot]);
    g_gl.PixelStorei(GL_PACK_ALIGNMENT, 4);
    g_gl.PixelStorei(GL_PACK_ROW_LENGTH, 0);
    g_gl.PixelStorei(GL_PACK_SKIP_ROWS, 0);
    g_gl.PixelStorei(GL_PACK_SKIP_PIXELS, 0);
    g_gl.ReadPixels(0, 0, w, h, kBgra, GL_UNSIGNED_BYTE, nullptr);
    g_res.pending[slot] = true;
    g_res.pending_qpc[slot] = static_cast<uint64_t>(qpc_now());
    state.count_copy();

    const uint32_t older = (slot + 1) % MELLO_HOOK_TEXTURE_COUNT;
    bool published = false;
    if (g_res.pending[older]) {
        g_gl.BindBuffer(kPixelPackBuffer, g_res.pbo[older]);
        const auto* pixels = static_cast<const uint8_t*>(g_gl.MapBuffer(kPixelPackBuffer, kReadOnly));
        if (pixels) {
            copy_flipped(pixels, older);
            g_gl.UnmapBuffer(kPixelPackBuffer);
            published = true;
        }
        g_res.pending[older] = false;
    }
    restore_state(saved);
    g_res.copy_ticks += static_cast<uint64_t>(qpc_now() - copy_started);
    ++g_res.copies;
    g_res.next = older;
    if (published) {
        state.publish_frame(older, g_res.pending_qpc[older]);
        state.signal_frame();
    }
}

void note_fault() {
    if (!g_disabled.exchange(true)) {
        HookState::instance().count_fault();
        HookState::instance().set_error(MELLO_HOOK_ERR_EXCEPTION);
        log_line("fault in wglSwapBuffers; GL capture is off and the game keeps running");
    }
    // The state cannot be trusted after a fault: leak it rather than call GL.
    g_res = Resources{};
}

BOOL WINAPI hooked_swap_buffers(HDC dc) {
    HookState::instance().count_present();
    if (!g_disabled.load(std::memory_order_relaxed) &&
        !g_in_capture.exchange(true, std::memory_order_acquire)) {
        __try {
            capture_swap(dc);
        } __except (EXCEPTION_EXECUTE_HANDLER) {
            note_fault();
        }
        g_in_capture.store(false, std::memory_order_release);
    }
    return g_real_swap_buffers(dc);
}

}  // namespace

bool install_gl_hooks(const MelloHookInfo&) {
    // Only a game that loaded opengl32.dll draws with it. The hook never
    // loads it: that would start a GL runtime inside a D3D game.
    g_opengl32 = GetModuleHandleA("opengl32.dll");
    if (!g_opengl32) return false;
    g_real_swap_buffers = reinterpret_cast<SwapBuffersFn>(GetProcAddress(g_opengl32, "wglSwapBuffers"));
    g_get_current_context =
        reinterpret_cast<GetCurrentContextFn>(GetProcAddress(g_opengl32, "wglGetCurrentContext"));
    g_get_proc_address =
        reinterpret_cast<GetProcAddressFn>(GetProcAddress(g_opengl32, "wglGetProcAddress"));
    if (!g_real_swap_buffers || !g_get_current_context || !g_get_proc_address) return false;

    DetourTransactionBegin();
    DetourUpdateThread(GetCurrentThread());
    DetourAttach(reinterpret_cast<PVOID*>(&g_real_swap_buffers),
                 reinterpret_cast<PVOID>(hooked_swap_buffers));
    const LONG result = DetourTransactionCommit();
    if (result != NO_ERROR) {
        HookState::instance().set_error(MELLO_HOOK_ERR_DETOUR);
        log_line("Detours refused the GL transaction: %ld", result);
        return false;
    }
    log_line("hooked wglSwapBuffers in opengl32.dll");
    return true;
}

}  // namespace mello_hook
