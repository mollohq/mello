// Direct3D 12 capture, on the DXGI present detour.
//
// The DXGI Present detour fires for a D3D12 swap chain as for any other, but a
// D3D12 back buffer cannot be copied on a D3D11 device. The copy needs the
// command queue the game presents on, and a D3D12 swap chain does not hand it
// out (IDXGISwapChain::GetDevice answers the ID3D12Device, and E_NOINTERFACE
// for ID3D12CommandQueue). So the hook detours ID3D12CommandQueue::
// ExecuteCommandLists and keeps the direct queues the game submits to.
//
// The copy goes into one of two shared committed textures on the game's
// device (D3D12_HEAP_FLAG_SHARED), on the game's queue, before the real
// Present: after the game's work for the frame, before the flip. A shared
// fence is signalled after each copy. The textures and the fence travel as NT
// handles of the game's process; the client duplicates them, opens them on its
// D3D11 device, and waits on the fence on the GPU before it reads a slot.
//
// The rules of streaming-reliability plan section 7.5 hold: every function
// below runs inside the callers' structured exception guard, nothing on the
// steady present path allocates, locks or logs, and the game's thread never
// waits on the GPU.

#pragma once

#include <dxgi.h>

#include <cstdint>

namespace mello_hook {

class HookState;

enum class D3d12Present {
    NotD3d12,  ///< not a D3D12 swap chain: the D3D11 path takes it
    Handled,   ///< a D3D12 swap chain: captured, skipped, or failed with last_error set
};

/// Runs the D3D12 capture for one present. `swap_identity` is the swap
/// chain's IUnknown; `other_area` is the back buffer area of the D3D11 target,
/// so a game with two swap chains keeps the larger one. Present thread only.
D3d12Present d3d12_capture_present(IDXGISwapChain* swap, IUnknown* swap_identity,
                                   const DXGI_SWAP_CHAIN_DESC& desc, uint64_t other_area,
                                   HookState& state);

/// The back buffer area of the D3D12 target, 0 when there is none.
uint64_t d3d12_target_area();

/// True when `swap_identity` is the D3D12 swap chain this path captures.
bool d3d12_is_target(IUnknown* swap_identity);

/// Marks the calling thread as inside the real Present of a D3D12 swap chain.
/// A direct queue that DXGI submits to on this thread then is the swap
/// chain's own queue, the one the copy must go on.
void d3d12_mark_presenting(bool presenting);

/// Releases the D3D12 capture resources when the GPU no longer uses them.
/// Called on the present thread when the client stops asking for frames.
/// Returns false when a copy is still in flight: the caller tries again at a
/// later present instead of waiting.
bool d3d12_release_when_idle();

/// After a fault: forget the resources without releasing them. A fault means
/// the state cannot be trusted, and a leak is better than a crash.
void d3d12_abandon();

}  // namespace mello_hook
