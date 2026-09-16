// m3llo game capture hook — shared protocol.
//
// This header is the whole contract between the hook DLL, which runs inside the
// game process, and libmello, which runs in the m3llo client. Both sides
// compile this same file. Nothing else crosses the boundary.
//
// Design rules that the layout must keep:
//  - Each field has one writer. The client writes the control and offset
//    fields before it injects. The hook writes the frame fields afterwards.
//  - The layout is fixed and versioned. A client must refuse a block whose
//    version or size does not match, because the two binaries ship in
//    different files and can differ after a failed update.
//  - Texture handles are legacy DXGI shared handles, stored as uint32_t. They
//    open with OpenSharedResource in any process on the same machine, from a
//    32-bit game into the 64-bit client, with no DuplicateHandle.
//  - The client creates the block, the events and the keepalive mutex before it
//    injects. The hook only opens them. A hook that finds no block does
//    nothing, which is what must happen if it is ever loaded by accident.
//
// Threading: the hook writes from the game's present thread. The client reads
// from its capture thread. `frame_index` orders every write: the hook fills a
// texture, then publishes the index last with a release store. The client reads
// the index first with an acquire load.

#ifndef MELLO_HOOK_PROTOCOL_H
#define MELLO_HOOK_PROTOCOL_H

#include <stdint.h>

#define MELLO_HOOK_PROTOCOL_VERSION 1u

// Two textures, used one after the other. The hook writes the one the client is
// not reading, so a slow client cannot see a half-written frame.
#define MELLO_HOOK_TEXTURE_COUNT 2u

// The hook stops capture when the client stops writing its heartbeat.
#define MELLO_HOOK_HEARTBEAT_TIMEOUT_MS 5000u

// Graphics API the hook captured the frame from.
enum MelloHookApi {
    MELLO_HOOK_API_NONE   = 0,
    MELLO_HOOK_API_D3D11  = 1,  // and D3D10, through DXGI
    MELLO_HOOK_API_D3D12  = 2,
    MELLO_HOOK_API_D3D9   = 3,
    MELLO_HOOK_API_OPENGL = 4,
};

// `last_error` values. The client reports these; the hook never shows anything.
enum MelloHookError {
    MELLO_HOOK_OK                 = 0,
    MELLO_HOOK_ERR_NO_DEVICE      = 1,  // could not get the device from the swap chain
    MELLO_HOOK_ERR_SHARED_TEXTURE = 2,  // could not create a shareable texture
    MELLO_HOOK_ERR_FORMAT         = 3,  // back buffer format cannot be shared
    MELLO_HOOK_ERR_COPY           = 4,  // the copy on the present path failed
    MELLO_HOOK_ERR_EXCEPTION      = 5,  // a detour body faulted and capture is off
    MELLO_HOOK_ERR_MULTISAMPLED   = 6,  // back buffer is multisampled, needs a resolve
    MELLO_HOOK_ERR_NO_OFFSETS     = 7,  // the client passed no usable offsets
    MELLO_HOOK_ERR_DETOUR         = 8,  // Detours refused the transaction
};

// `flags`
#define MELLO_HOOK_FLAG_HDR      0x1u  // the back buffer format is HDR
#define MELLO_HOOK_FLAG_CPU_COPY 0x2u  // GPU sharing failed; frames come through memory
#define MELLO_HOOK_FLAG_SRGB     0x4u  // the back buffer view is sRGB

#pragma pack(push, 8)
// The shared memory block. Fixed layout, checked by the static assertions below.
typedef struct MelloHookInfo {
    // --- Identity. protocol_version and struct_size come from the client; the
    // hook checks them and fills in the rest. ---
    uint32_t protocol_version;   // MELLO_HOOK_PROTOCOL_VERSION
    uint32_t struct_size;        // sizeof(MelloHookInfo)
    uint32_t hook_pid;           // the game process id, written by the hook
    uint32_t hook_bitness;       // 32 or 64, for the client's log

    // --- Frame description. Written by the hook, read by the client. ---
    uint32_t api;                // MelloHookApi
    uint32_t width;
    uint32_t height;
    uint32_t dxgi_format;        // DXGI_FORMAT of the shared textures
    uint64_t adapter_luid;       // LUID of the adapter that owns the textures
    uint32_t shared_handles[MELLO_HOOK_TEXTURE_COUNT];  // legacy shared handles
    uint32_t texture_generation; // bumps when the handles change (resize, reset)
    uint32_t texture_index;      // which handle holds the newest frame
    uint32_t flags;              // MELLO_HOOK_FLAG_*
    uint32_t last_error;         // MelloHookError

    // --- Control. Written by the client, read by the hook. ---
    uint32_t capture_enabled;    // 1 asks the hook to capture, 0 to stop
    uint32_t client_pid;

    // --- Frame publication. Written by the hook, last and with a release. ---
    uint64_t frame_index;        // count of captured frames; 0 means none yet
    uint64_t frame_qpc;          // QueryPerformanceCounter at the present call

    uint64_t heartbeat_qpc;      // the client updates this; see the timeout above

    // --- Counters for telemetry. Written by the hook. ---
    uint64_t frames_dropped;     // presents the copy could not keep up with
    uint64_t faults;             // detour bodies that faulted

    // --- Present function offsets, from each module's base address. Written by
    // the client before injection, from the offsets helper. The hook adds them
    // to the loaded module base. The hook never builds a probe device of its
    // own inside the game: that is what these offsets are for. Zero means the
    // client has no offset for that function. ---
    uint32_t offsets_valid;      // 1 when the offsets below were filled in
    uint32_t offsets_reserved;
    uint64_t off_dxgi_present;                 // IDXGISwapChain::Present, from dxgi.dll
    uint64_t off_dxgi_present1;                // IDXGISwapChain1::Present1
    uint64_t off_dxgi_resize_buffers;          // IDXGISwapChain::ResizeBuffers
    uint64_t off_d3d12_execute_command_lists;  // from d3d12.dll
    uint64_t off_d3d9_present;                 // IDirect3DDevice9::Present, from d3d9.dll
    uint64_t off_d3d9_present_ex;              // IDirect3DDevice9Ex::PresentEx
    uint64_t off_d3d9_swapchain_present;       // IDirect3DSwapChain9::Present
    uint64_t off_d3d9_reset;                   // IDirect3DDevice9::Reset
    uint64_t off_d3d9_reset_ex;                // IDirect3DDevice9Ex::ResetEx
    uint64_t off_opengl_swap_buffers;          // wglSwapBuffers, from opengl32.dll

    // --- Frames through memory. Written by the hook, and only when it sets
    // MELLO_HOOK_FLAG_CPU_COPY. A Direct3D 9 game reads back its own render
    // target and writes the pixels into a second shared block, because a D3D9
    // surface cannot be opened on the client's D3D11 device. ---
    uint32_t cpu_frame_bytes;    // size of one frame, 0 when frames are textures
    uint32_t cpu_pitch;          // bytes per row in that frame

    uint64_t reserved[5];
} MelloHookInfo;
#pragma pack(pop)

#if defined(__cplusplus) && __cplusplus >= 201103L
static_assert(sizeof(MelloHookInfo) == 248, "MelloHookInfo layout changed: bump the protocol version");
static_assert(sizeof(MelloHookInfo) % 8 == 0, "MelloHookInfo must stay 8-byte aligned");
#endif

// --- Kernel object names -----------------------------------------------------
//
// One set per game process. `Local\` keeps them in the user's session, so no
// elevation and no clash between sessions. Both sides build the same names from
// the game's process id.

#define MELLO_HOOK_NAME_INFO      "Local\\mello_hook_info_%u"
// Frames through memory, one block per game. The hook creates this one,
// because only the hook knows how big a frame is.
#define MELLO_HOOK_NAME_FRAMES    "Local\\mello_hook_frames_%u"
#define MELLO_HOOK_NAME_READY     "Local\\mello_hook_ready_%u"
#define MELLO_HOOK_NAME_FRAME     "Local\\mello_hook_frame_%u"
#define MELLO_HOOK_NAME_STOP      "Local\\mello_hook_stop_%u"
#define MELLO_HOOK_NAME_KEEPALIVE "Local\\mello_hook_alive_%u"

// The window message the injection helper posts to wake the hooked thread.
#define MELLO_HOOK_WAKE_MESSAGE "mello_hook_wake"

#ifdef __cplusplus
#include <cstddef>
#include <cstdio>

namespace mello_hook {

// Builds one of the names above for `pid`. `out` needs 64 characters.
inline void object_name(char* out, size_t out_size, const char* pattern, uint32_t pid) {
    std::snprintf(out, out_size, pattern, pid);
}

}  // namespace mello_hook
#endif

#endif  // MELLO_HOOK_PROTOCOL_H
