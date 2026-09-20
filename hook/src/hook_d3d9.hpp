// Direct3D 9 present hooks.
//
// A D3D9 surface cannot be opened on the client's D3D11 device, so this path
// reads the render target back into system memory and writes the pixels into a
// second shared block. Plan 3.2 asks for this path first, and for the shared
// GPU path after it: that one needs a D3D9Ex device, and a game that made a
// plain D3D9 device cannot share at all without reaching into the device's
// internals.
//
// The same rules as the DXGI hooks: never crash the game, never allocate on the
// present path, and do nothing at all when the client is not asking.

#pragma once

#include "mello_hook_protocol.h"

namespace mello_hook {

/// Detours the Direct3D 9 present and reset functions named by the offsets in
/// `info`. False means d3d9.dll is not loaded in this process, or the client
/// has no offsets for it.
bool install_d3d9_hooks(const MelloHookInfo& info);

/// Releases the read-back surfaces. The present thread owns them, so this only
/// records the intent, as the DXGI side does.
void stop_d3d9_capture();

}  // namespace mello_hook
