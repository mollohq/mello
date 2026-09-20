// DXGI present hooks (Direct3D 11 and 10).
//
// Everything in here except `install` and `stop_capture` runs on the game's
// present thread, inside a structured exception guard. The rules from plan 3.7
// apply to every line of it: never crash the game, never allocate, never take a
// lock the game could hold, and do nothing at all when the client is not
// asking for frames.

#pragma once

#include "mello_hook_protocol.h"

namespace mello_hook {

// Detours the present functions named by the offsets in `info`. The offsets
// come from the client, which got them from the offsets helper outside the
// game. False means the hook has nothing to work with; `last_error` says why.
bool install_dxgi_hooks(const MelloHookInfo& info);

// Releases the capture resources. Called from the hook thread when the client
// stops asking for frames. The detours stay in place: taking them out while a
// game thread could be inside one is a crash risk (plan 3.7).
void stop_dxgi_capture();

}  // namespace mello_hook
