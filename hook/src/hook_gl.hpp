// OpenGL present hook.
//
// An OpenGL game presents with SwapBuffers, which ends in opengl32.dll's
// exported wglSwapBuffers. The hook detours that export (GetProcAddress on the
// loaded opengl32.dll: an export is not a probe, and it is the same function
// whatever the driver). Inside it the game's context is current on the
// calling thread, which is the only place GL may be called.
//
// Two ways to the client, chosen at each build of the capture resources:
//  - WGL_NV_DX_interop2: a D3D11 device made in the game on the client's
//    adapter owns two legacy-shared BGRA textures; GL renders into them
//    through the interop and a framebuffer blit (flipped: GL rows go bottom
//    up). The client opens them exactly like a DXGI game's textures. No
//    read-back.
//  - Otherwise, memory: the back buffer is read into one of two pixel buffer
//    objects, and the previous one is mapped and copied (flipped) into the
//    frame block the Direct3D 9 path uses. The read is asynchronous, so the
//    game's thread does not wait for it; a frame arrives one due frame late.
//
// The rules of streaming-reliability plan section 7.5 hold: the detour body
// runs inside a structured exception guard, a fault turns capture off for
// good, and the game's GL state (bindings, scissor, sRGB write, pack state) is
// put back exactly as it was.

#pragma once

#include "mello_hook_protocol.h"

namespace mello_hook {

/// Detours wglSwapBuffers when opengl32.dll is loaded in this process. False
/// when it is not: the game draws through another API.
bool install_gl_hooks(const MelloHookInfo& info);

}  // namespace mello_hook
