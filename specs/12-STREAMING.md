# Streaming

> **Component:** libmello (C++) · mello-core (Rust) · mello-client (Rust/Slint) · Backend (Go/Nakama)
> **Status:** H.264 RTP video path implemented through the SFU (the only transport); production SFU deployed; client runtime certification remains
> **Related:** [03-LIBMELLO.md](./03-LIBMELLO.md), [02-MELLO-CORE.md](./02-MELLO-CORE.md), [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md), [EXTERNAL-SFU.md](./EXTERNAL-SFU.md)

---

## 1. Goals

Ship 1080p60 game streaming comparable to Discord/Parsec with low idle RAM and hardware-composited video in the desktop client: hardware-accelerated encode/decode, sub-60ms WAN latency, and stable UI render cadence. Favor visible quality loss (artifacts, lower bitrate) over lag or stalling.

---

## 2. Layer Overview

The streaming system is split across four layers. Understanding ownership boundaries is the most important thing for working on this stack.

```
┌─────────────────────────────────────────────────────────────────┐
│  mello-client (Rust/Slint)                                      │
│  DComp composition presenter, geometry sync, stream card UI      │
├─────────────────────────────────────────────────────────────────┤
│  mello-core  (Rust)                                             │
│  Stream lifecycle, native RTP transport, REMB congestion,       │
│  PLI/IDR recovery, access-unit polling, frame-lifecycle, telemetry │
├─────────────────────────────────────────────────────────────────┤
│  libmello  (C++)                                                │
│  Hardware capture, GPU color conversion, hardware encode/decode, │
│  decoded-frame ring, native frame callback (NT shared handles)   │
├─────────────────────────────────────────────────────────────────┤
│  mello-sys  (Rust FFI bindings, auto-generated via bindgen)     │
│  Thin unsafe bridge between mello-core and libmello             │
└─────────────────────────────────────────────────────────────────┘
```

mello-core never touches pixel memory. libmello owns native RTP mechanics (packetization, pacing, repair, RTCP, and AU assembly), while mello-core owns topology, congestion, and recovery policy. mello-client owns presentation and composition. `mello-sys` is the FFI membrane.

---

## 3. Host Pipeline

The host captures frames, converts them, encodes them, and hands complete Annex-B access units to the Rust layer, which forwards them through native H.264 RTP senders.

```
Capture → GPU Preprocess → Encode Queue → Encode Thread → Stream Manager → RTP egress
(DXGI/WGC) (BGRA→NV12)    (bounded ring)   (NVENC async)   (pacing, REMB)   (SFU)
```

### 3.1 Capture

Four methods, tried in order by the capture ladder:

| Backend | API | Notes |
|---------|-----|-------|
| **Hook** | m3llo game capture hook, inside the game | The game's own back buffer. The only method that sees exclusive fullscreen. Needs permission; see 3.1.1. |
| **WGC** | `Windows.Graphics.Capture`, `CreateForWindow` | The game window. Cannot see exclusive-fullscreen content. |
| **WGC-Monitor** | `Windows.Graphics.Capture`, `CreateForMonitor` | Fallback when window capture fails. |
| **DXGI-DDI** | `IDXGIOutputDuplication` | The game window's monitor. Cannot see exclusive-fullscreen content either. Last, for the reason below. |

Without permission for the hook, the ladder starts at WGC.

`ProcessCapture` owns the ladder. Given a PID it finds the main game window
(`EnumWindows`, largest restored-area, non-toolwindow) and tries the methods in
one order for every game: **the hook when it is allowed, then WGC window, WGC
monitor, DXGI desktop duplication**.

Measured on 2026-09-16 against Unigine Heaven (Direct3D11, borderless
fullscreen, 3440x1440, NVIDIA):

| Method | Delivered fps | Present-to-capture p50 / p95 / p99 |
|---|---|---|
| WGC window | 48 | 1 ms / 1 ms / 1 ms |
| DXGI-DDI | 1 frame in 20 s, then nothing | n/a |

Desktop duplication is last because of what the same runs showed:
`AcquireNextFrame` blocks inside the NVIDIA display driver (`nvwgf2umx.dll`) and
ignores its timeout. That wedged thread also leaves the shared D3D11 device
unusable, so every later capture attempt on that device blocks. The ladder marks
the device poisoned and stops using it.

**A method fails only on evidence.** The evidence is:

| Evidence | Applies to | Rule |
|---|---|---|
| No first frame | Every method | 2 s with no frame at all, or 15 s while the game has drawn nothing (see below) |
| Too few frames | DXGI only | Under 3 frames in 3 s. Duplication delivers a frame for every change on screen, so silence under a game that presents means it is blind. WGC delivers a frame only when the captured content changes, so silence there is a static game. |
| Backend stopped for good | Every method | Duplication rebuild gave up, or the capture item closed |
| Game entered exclusive fullscreen | Every method | `SHQueryUserNotificationState`. Rebuilds the current method and restarts its deadline |

While the game is minimized every deadline is held, because nothing can capture
a minimized game.

**A game that has drawn nothing is not a failed capture method.** A person
starts the stream and then goes to their game: switching window, loading a
level and taking fullscreen take seconds, and no method has anything to show in
that time. Moving the ladder on there is worse than waiting, because monitor
capture then delivers the desktop, keeps delivering, and nothing ever moves the
ladder back. That is what a user saw on 2026-09-16: their viewer watched the
whole desktop with the game as a small window inside it.

Only the hook can tell the difference, because only the hook sees the game's
presents from inside the game: it counts them in `presents_seen`, whether or
not it captured them. No presents at all means the game has drawn nothing, and
the first-frame deadline becomes 15 s. It stays bounded, because a game that
draws through an API the hook does not cover looks exactly the same.

**The ladder walks back up on evidence.** It only ever walks down by itself,
since the method it settles on keeps delivering. When the game enters exclusive
fullscreen, the best method becomes possible again, so the ladder restarts from
the top.

**A move starts the next method before it stops the current one.** Only one
method reaches the pipeline at a time. A walk back up (exclusive fullscreen,
and the 30 s retry while the game stays there) keeps the current method on the
stream until the next one delivers its first frame, for at most 2 s. When the
next method delivers nothing in that time, it stops, and the current method
never stopped. A hook attempt takes seconds (injection, then the game's next
present), and stopping first showed viewers no frames for all of that time.
A move down the ladder (the current method failed) swaps as soon as the next
method starts, because the current one has nothing to keep.

Every ladder move forces a keyframe. When every method has delivered nothing,
the ladder goes back to the first method, waits 30 s, and tries them all again.
The user sees an error only with proof that the game is rendering: the game is
in exclusive fullscreen, or the capture device is stuck. Host stats then carry
`cap_failed`, the method history rides in `cap_hist`, and the client sends one
`StreamError`. Without that proof the stream is quiet, not broken, and the user
is not told anything. Only the game capture hook (3.1.1) sees an
exclusive-fullscreen game, so such a game streams only when the hook policy
allows it.

**Target exit ends the stream.** Each backend holds the target process object
through an open handle, so pid reuse cannot confuse it, and reports
`target_exited` in host stats. The host manager polls it at 1 Hz; on exit the
host stops and calls `stop_stream`, and viewers leave through the normal
crew-event path.

This is deliberately not a capture state. Every capture state recovers: the
game is minimized, or loading, or drawing through an API no method can see. A
quit game is final, and from outside it looks identical to a tabbed-out one.
Folding them together would either end streams on an alt-tab or hold a stream
open on a game that is gone. A minimized target only pauses; monitor capture
has no target and never exits. Any ambiguous answer reads as alive, so a
stream never ends on uncertainty.

### 3.1.1 Game capture hook

The hook is the first ladder step, and the only method that sees an
exclusive-fullscreen game. It is off unless the caller allows that game.

**How a frame travels.** The hook DLL runs inside the game. For a DXGI game it
detours `IDXGISwapChain::Present`, `Present1` and `ResizeBuffers`. For a
Direct3D 9 game it detours `Present`, `PresentEx`, the swap chain `Present`,
`Reset` and `ResetEx`. On each present it copies the back buffer into one of
two shared textures and signals an event. `HookCapture`
(`libmello/src/video/capture_hook.cpp`) opens those textures on the encoder's
device, copies the newest one, and hands it to the pipeline as any other
backend does. Only the Direct3D 9 fallback below copies frames through system
memory.

**Binaries** (`hook/`, its own CMake project because it builds for x86 as well
as x64 and links the static CRT):

| Binary | Runs where | Does what |
|---|---|---|
| `mello-hook{32,64}.dll` | Inside the game | Detours DXGI, D3D9 and OpenGL present (and D3D12 `ExecuteCommandLists`), copies, signals |
| `mello-inject{32,64}.exe` | Its own process | `SetWindowsHookEx(WH_GETMESSAGE)` on the game's window thread |
| `mello-offsets{32,64}.exe` | Its own process | Prints present-function offsets |

**The shared block** (`hook/include/mello_hook_protocol.h`, protocol version 4)
is the whole contract. The client creates it, the events and the keepalive
before it injects; the hook only opens them. Each field has one writer. The
client writes the LUID of its adapter into `client_adapter_luid`, so a Direct3D
9 game can tell whether it may share. Textures are shared with legacy DXGI
handles (`D3D11_RESOURCE_MISC_SHARED`, `GetSharedHandle`), stored as
`uint32_t`, so a 32-bit game's texture opens in the 64-bit client with no
`DuplicateHandle`.

**The hook copies at the stream's rate, not the game's.** The client writes
its `target_fps` into the block, and the hook copies a frame only when one is
due by libmello's cadence rule (`frame_cadence.hpp`, compiled into the hook
too). A present between two due frames costs the game one check. A 144 fps
game on a 60 fps stream gives 60 copies a second (measured with the fakegame
at `--fps 144`: 144 presents, 60 copies, 60 delivered, Direct3D 11 and 9);
before, every present was copied, and on the Direct3D 9 memory path every
present stalled the game's render thread for a read-back. `frames_copied`
counts the copies. The client keeps its own throttle with the same rule; two
throttles with the rule in series pass every frame.

**Offsets, not probes.** The hook never creates a device inside a game to find
`Present`. The offsets helper builds a throwaway D3D11 swap chain in its own
process, reads the addresses out of the COM virtual function table, and prints
them as offsets from `dxgi.dll`. The client caches them against that file's
version and writes them into the shared block before injection.

**Safety rules the code keeps** (streaming-reliability plan §7.5): `DllMain`
starts a thread and returns; every detour body runs in a structured exception
guard and a fault turns capture off for good; nothing on the present path
allocates, locks or logs; the DLL pins itself and the detours are never
removed, because a game thread can be inside one; the hook stops capturing
5 s after the client's heartbeat stops.

**Permission.** Two gates, both needed. Design and reasons:
`mello-backlog/plans/streaming-reliability.md` §8 and
`game-identity-hook-policy.md`.

1. **The backend policy allows this game** (mello-core). The `start_stream`
   response carries a `capture` block, which the backend reads from Nakama
   storage `capture_config/hook_policy` on every call:

   | Field | Meaning |
   |---|---|
   | `hook_enabled` | Global kill switch. |
   | `hook_allow_ids` | IGDB ids that may be hooked. |
   | `hook_deny_ids` | IGDB ids that are never hooked. Deny wins. |
   | `policy_version` | `<UTC time>:admin:<name>`, for the logs. |

   The client takes `igdb_id` from the catalogue lookup of the process it
   captures: the `stream_games` row of that pid. `hook_allowed_for_game`
   refuses when there is no block, the switch is off, the id is 0 (the
   catalogue does not know the process), or the id is denied. Otherwise the id
   must be on the allow list. The result is `allow_hook`, and the client logs
   `Hook policy: exe=… igdb_id=… allow_hook=… policy_version=…`.

   The lists come from the hook catalogue (`games.csv`): `hook` → allow,
   `no_hook` → deny, `hook_review` → neither. Counter-Strike 2 is always
   denied. The admin tool (`mello-admin`, Hook policy page) edits the stored
   policy through `admin_capture_policy_get` and `admin_capture_policy_set`.
   Those RPCs accept the `http_key` only, refuse a client session, and write
   only if the storage version still matches the admin's read.
   `hook_policy_seed.json` holds the catalogue lists for tests and has the
   switch off.

   With no stored policy, or with `hook_enabled: false`, nothing is hooked.
   The switch stays off until the anti-cheat audit in
   `game-identity-hook-policy.md` §6 is done.

   The environment variable `MELLO_HOOK_ALLOW_EXE` names one executable and
   skips this gate for it, on a developer machine. Gate 2 still applies.
2. **The run-time checks** in `hook_policy.cpp`, on every stream start: the
   process can be opened for read, no anti-cheat module is loaded in it, no
   anti-cheat service is running, it is not Store-packaged, not a Chromium
   shell, and not elevated. Any one of these refuses the hook, and the ladder
   falls back to screen capture.

**The shared D3D11 context is thread protected.** Capture threads copy frames
into the immediate context and the encode thread converts them. A D3D11
immediate context is not thread safe by itself, and without
`ID3D11Multithread::SetMultithreadProtected` the video processor refuses input
views with `E_INVALIDARG` while another thread is inside a copy. Measured on
2026-09-16 against a Direct3D 9 game: capture ran at 50 fps and every frame
failed to convert. `create_d3d11_device` turns it on for every backend.

**Direct3D 9 shares textures when it can.** The hook checks the game's device
each time it builds its capture resources: on the first present, and after a
change of back-buffer size or format. A D3D9Ex device on the client's adapter
(its LUID equals `client_adapter_luid`) gets two shared textures on the game's
own device.
`StretchRect` resolves the back buffer into them on each present: no
read-back, no CPU copy. Direct3D 9 queues that copy, and the client's device
has nothing to wait on, so each slot has an event query. The hook hands a slot
to the client only after its copy has retired, and it checks this at the next
present, so the game's thread never waits. Without that wait the client opens
the texture and reads zeros (`test_d3d9ex_share.cpp`). Measured with the
fakegame (Direct3D 9, windowed, 20 s ladder bench): Hook on 20 of 20 samples,
48 fps, 1 ms present-to-capture at p50 and p99, no drops.

**Otherwise Direct3D 9 travels through memory.** A plain D3D9 device cannot
share a surface, and a shared handle is valid on no other adapter. For those
games, on each present the hook reads the back buffer back with
`GetRenderTargetData` into a system-memory surface, and copies the pixels into
a second shared block (`Local\mello_hook_frames_<pid>`, two slots). The
read-back stalls the game's render thread. The client uploads them into a
texture and the pipeline sees the same thing as from any other backend. The
hook sets `MELLO_HOOK_FLAG_CPU_COPY` and the `cpu_frame_bytes` and `cpu_pitch`
fields; those are how the client knows which transport to use. The back buffer
goes through a single-sample render target first (`StretchRect`), because a
multisampled surface cannot be read back at all, and a game's back buffer often
is one. `Reset` and
`ResetEx` are detoured as well: a reset changes the back buffer, so the
read-back surfaces are dropped and the next present rebuilds them.

**Direct3D 12.** The DXGI `Present` detour fires for a D3D12 swap chain too,
but the copy needs the command queue the game presents on, and a D3D12 swap
chain does not give it out (`GetDevice` answers the `ID3D12Device`, and
`E_NOINTERFACE` for `ID3D12CommandQueue`). So the hook detours
`ID3D12CommandQueue::ExecuteCommandLists`. Its address comes from a command
queue made once on the game's own device and released at once: a game that
ships the Agility SDK runs its own `D3D12Core.dll`, where an offset of the
system DLL points anywhere. The detour keeps the direct queues the game
submits to (at most 8, each referenced once and never released, so the
present path reads them with no lock). DXGI submits on the swap chain's own
queue inside `Present`, so a queue seen on the presenting thread during
`Present` is the right one; else the last direct queue of the same device.

On each due present, before the real `Present`, the hook records a copy of the
back buffer (barriers present to copy source and back) into one of two
committed textures with `D3D12_HEAP_FLAG_SHARED`, executes it on that queue,
and signals a shared fence. A slot is reused only when its last copy has
retired; else the frame is skipped, never waited for. The textures and the
fence travel as NT handles of the game's process (`MELLO_HOOK_FLAG_NT_HANDLES`,
protocol version 5). The client duplicates them (`PROCESS_DUP_HANDLE` on the
game), opens them with `OpenSharedResource1` and `OpenSharedFence`, and before
each read waits on the GPU (`ID3D11DeviceContext4::Wait`) for
`frame_fence_value`, which also names the slot. Measured with the fakegame
(`--d3d12`, 640x360): the pixels arrive, 60 delivered a second at 144
presents, and the loopback decodes the colour.

**OpenGL.** The hook detours `wglSwapBuffers`, an export of `opengl32.dll`
that it resolves by name, only when the game has loaded `opengl32.dll` (the
hook links no GL library). Inside the detour the game's context is current.
With `WGL_NV_DX_interop2`, the hook makes a D3D11 device on the client's
adapter (its LUID is in the block), two legacy-shared BGRA textures, registers
them with GL, and blits the default framebuffer into one (flipped: GL rows go
bottom up), then unlocks and flushes. The client opens them as it opens a
DXGI game's textures. Without the interop (or with
`MELLO_HOOK_GL_READBACK=1`, which sets `MELLO_HOOK_OPTION_GL_READBACK`, protocol
version 6), the back buffer is read into one of two pixel buffer objects and
the one read a due frame earlier is mapped and copied, flipped, into the
memory frame block (`MELLO_HOOK_FLAG_CPU_COPY`): the read is asynchronous, so
the game's thread does not wait, and a frame arrives one due frame late. Every
piece of GL state the copy touches (framebuffer bindings, the default
framebuffer's read buffer, scissor test, sRGB write, pack buffer and pack
state) is put back, and the hook leaves no GL error behind.

Measured on the game's thread with the fakegame (`--opengl`, 60 copies a
second, this NVIDIA machine shared with other builds):

| Path | 640x360, game at 144 fps | 640x360, game uncapped | 1920x1080, game uncapped |
|---|---|---|---|
| Interop | 0.26 ms a copy | 1.1 ms | 1.0 ms |
| Memory | 0.18 ms | 0.36 ms | 1.1 ms |

The interop lock waits for the GPU when the game keeps it busy; the memory
path's cost is the copy of the mapped pixels.

**What works today:** DXGI swap chains, which covers Direct3D 12, 11 and 10;
Direct3D 9: shared textures for a D3D9Ex game on the client's adapter,
memory for every other D3D9 game; and OpenGL. Both for 32-bit and 64-bit
games. Vulkan games use the WGC steps by decision
(streaming-reliability plan §9).

**Deferred start:** If the target window is minimized at stream start (user tabbed out to launch the stream), capture waits. The monitor thread polls until the window is restored, then initializes the backend. Width/height return restored dimensions during the wait so the encoder can pre-initialize. This matches Discord's behaviour.

**Output rate is clamped at the pipeline, not trusted from the backend.** Capture
backends deliver at the *display's* refresh rate — there is no "give me 60" knob
in Desktop Duplication or WGC — so every backend carries its own throttle, and
those throttles have been measured over-delivering (83 fps against a 60 fps
target on a 165 Hz host). The encoder then faithfully encodes all of them and
overshoots its bitrate by exactly that ratio, because rate control budgets bits
per frame from the framerate it was configured with:

> 5000 kbps x (83/60) = 6900 kbps — matching the 6697 kbps observed in the field.

`VideoPipeline::decimation_accepts` therefore clamps unconditionally to the
configured rate. It is an **accumulator**: the deadline advances by exactly one
output interval per accepted frame and is never reset to the arrival time.
Resetting is what collapses the output onto the source's quantisation — with a
12 ms source and a 16.7 ms target the naive form can only emit every source
frame or every other one, never the 60-of-83 that actually equals 60 fps. No
tolerance value fixes that; the formula cannot express the answer. A small fixed
tolerance (1/8 of the *target* interval) absorbs arrival jitter, and a stall
resyncs the deadline rather than emitting a catch-up burst into a 2-deep encode
queue.

Skipped frames are released before the GPU colour conversion, so a decimated
frame costs essentially nothing — which is the whole reason to drop at capture
rather than at the encode queue, where an over-fed queue evicts frames you
wanted.

**Minimum encodable size.** Hardware encoders reject frames below ~145x49
(NVENC H.264) with a generic unsupported-parameter error, which surfaces as
"no hardware encoder available" and reads as a broken GPU. `start_host` refuses
earlier and names the actual size. The usual cause is a capture target that is
not what the user thinks it is: a minimized window, where WGC returns the ~160x28
iconic size, or an auxiliary window sharing the game's title. The WGC hot-swap
path refuses a minimized target for the same reason and waits for restore —
a fullscreen game exiting to the desktop minimizes it, so that path is common.

**Idle keepalive:** DXGI and WGC deliver a frame only when pixels change, so a
quiet screen sends no video. After 500 ms without a new frame the encode thread
re-encodes the last picture at 2 fps, and a keyframe request re-encodes it as an
IDR at once. Without this a viewer who joined a static stream saw black: the IDR
waited for a frame that never came. Counted as `idle_rep_hz` in host stats.

**Adaptive DXGI throttle:** DXGI delivers at the monitor's refresh rate (60–360 Hz). We only want `target_fps` (typically 60). On startup, we calibrate the monitor's vsync interval from the first two acquired frames, then set a deadline of `target_interval - half_vsync`. This ensures we accept the closest vsync that satisfies the target on any refresh rate, without over- or under-delivering.

**macOS:** `ScreenCaptureKit` (SCK) backend exists for macOS capture.

### 3.2 GPU Preprocessing

BGRA→NV12 conversion via a D3D11 compute shader. Uses a 3-slot NV12 ring buffer so the convert output doesn't alias with an in-flight encode input. Typical `convert_ms` is 0.1–0.3ms. Also handles GPU downscale when the capture resolution exceeds the target encode resolution.

The encode size keeps the source aspect (`VideoPipeline::encode_size_for`):

| Rule | Example |
|---|---|
| The height is the preset height. The width follows the source aspect, rounded to a multiple of 16. | 1920x1200 on 1080p gives 1728x1080. 1440x1080 on 720p gives 960x720. |
| The size never goes above the capture size. | 1280x720 on 1080p stays 1280x720. |
| Over H.264 level 4.2 (8704 macroblocks per frame, 522240 per second), the height goes down until the size fits. The host logs a warning. | 3440x1440 on 1080p60 gives 2288x960, not 2560x1080 (10880 macroblocks). |

The host reports the size through `mello_stream_get_host_resolution`.

**The source size can change mid-stream; the encode size does not.** A window
resize, a display mode change, or a ladder move to a method of another size
gives frames of a new size. Every backend follows it: WGC makes its frame pool
again at the new content size (the frame of the change is dropped), DXGI
rebuilds duplication at the new mode, the hook publishes new textures. The
preprocessor scales the whole new source into the fixed encode size with the
aspect kept: black bars (studio black) fill the rest, nothing is cropped. An
aspect within 1 % of the encode aspect fills the output, because that is the
rounding of `encode_size_for`. `MelloStreamStats` carries `source_width`,
`source_height` and `source_size_changes` (changes after the first frame); a
change of the counter is the signal for the stage 4 geometry controller.

### 3.3 Encode Queue

A dedicated `encode_thread` pulls from a bounded ring queue (`ENCODE_QUEUE_CAP = 2`). When the queue is full, the oldest job is evicted (newest-wins). This decouples the capture callback thread from the potentially-blocking encode path.

### 3.4 Hardware Encode

NVENC tries P4+ULL (Ultra Low Latency) first, then P1+ULL, then P1+LL. The effective preset is logged at init.

**Async mode:** The encoder initializes with `enableEncodeAsync = 1` and registers a Windows completion event. `nvEncEncodePicture` returns immediately while the GPU works; the encode thread waits on the event before calling `nvEncLockBitstream`. Falls back to synchronous mode if the driver doesn't support async.

Rate control is VBR with 1.25× max headroom. The VBV spans ~0.5 s of the max rate (floored at 4 frames of bits, `vbvInitialDelay = vbv/2`) so IDRs are not rate-starved. Bitrate reconfigures reuse the full init-time NVENC config (the driver does not merge sparse configs on re-init) and force an IDR only on down-steps > 25%. Texture registration is cached per NV12 ring slot so `nvEncRegisterResource` runs once per slot, not per frame. `repeatSPSPPS = 1` ensures every keyframe is self-contained.

**Other encoder backends:** AMF (AMD), QSV/oneVPL (Intel), VideoToolbox (macOS) exist in the codebase but are less battle-tested than NVENC.

**Adaptive cost tiers.** Quality features that are free on a current GPU are not
free on an older one, and the encode queue is two deep with newest-wins
eviction — so an encoder that misses the frame budget does not produce a
slightly worse picture, it silently drops frames. `Encoder::reduce_cost_tier()`
gives features up one at a time when the pipeline measures that happening:

| Tier | Configuration |
|------|---------------|
| 0 | two-pass full-resolution + temporal AQ + spatial AQ 8 (default) |
| 1 | single-pass + temporal AQ + spatial AQ 8 |
| 2 | single-pass, no temporal AQ, spatial AQ 4 |

`VideoPipeline::encoder_is_overloaded` decides, from queue-drop ratio (>2% of
offered frames — the direct symptom) or mean encode time (≥80% of the frame
interval — the leading indicator), over a minimum 60-frame window so a startup
hitch cannot permanently downgrade quality. Tiers are monotonic within a
session: a GPU that could not keep up has not improved, and climbing back would
oscillate against the load being escaped. Each step forces an IDR and persists
into `base_config_`, so a later bitrate reconfigure does not resurrect the
features that were given up.

Driven by measurement rather than a GPU allowlist, so it covers hardware that
has never been tested. AMF and QSV have their own tiers, driven by the same
overload signal; the pipeline stops at each backend's `max_cost_tier()`:

| Tier | AMF (H.264) | QSV (oneVPL) |
|------|-------------|--------------|
| 0 | init: ultra-low-latency usage, pre-analysis off, the usage's quality preset | TargetUsage 4 (balanced) |
| 1 | quality preset speed | TargetUsage 6 |
| 2 | also VBAQ, high motion quality boost and pre-analysis off | TargetUsage 7 (best speed) |

AMF sets the properties and calls `ReInit` (some are static on some drivers);
a step that changes nothing (the usage default was already the cheaper one)
goes on to the next. QSV applies TargetUsage with `MFXVideoENCODE_Reset` and
keeps it in its parameters, so a later bitrate Reset does not take it back.
Both force an IDR. A step the driver refuses ends the tiers for the session,
as on NVENC. VideoToolbox has no tiers. The AMF and QSV tiers are compiled and
unit tested only: there is no AMD or Intel host in the lab.

### 3.5 Encoded Packet Handoff

The encode thread's `packet_cb_` fires with the encoded NALU bytes. This callback was set up by `mello-core` via `mello_stream_start_host` — it sends the bytes over an mpsc channel (capacity 32) to the Rust `StreamManager`.

---

## 4. Stream Manager (Host-side Rust)

`mello-core::stream::manager::StreamManager` is the host-side control loop. It receives encoded Annex-B access units from libmello and sends them through native RTP sinks (`PacketSink::send_video`).

### What it does each tick:

1. **Drain video packets** from the mpsc channel (bounded coalescing to avoid starvation).
2. **Send** complete access units via `PacketSink::send_video` (native RTP packetization in libmello).
3. **Poll RTCP feedback** (PLI, REMB) from sinks and request host keyframes or adjust bitrate.
4. **Apply REMB** — the SFU forwards one aggregated target from the relay path, under the synthetic viewer id `sfu`.
5. **Send game audio** — Opus packets from loopback capture via `PacketSink::send_audio`.
6. **Emit telemetry** every second: `video_in_hz`, `audio_in_hz`, `audio_out_hz`, `send_fail_*_delta`, `recovery_mode`, queue depths, `bitrate_kbps`.

### Recovery policy

- **Queue-pressure keyframe:** If the video queue grows too large (severe coalescing), force an IDR. Rate-limited.
- **Viewer-requested keyframe:** Forwarded from control packets, rate-limited.
- **Recovery mode:** A reference-chain gap, queue overflow, or failed accepted AU enters an IDR gate. Queued/dependent deltas are dropped and one rate-limited local keyframe request is emitted; deltas remain gated until a complete IDR arrives.

---

## 5. Video Transport (H.264 RTP)

Video uses **H.264 RTP/RTCP** for media — no custom `StreamPacket` framing or DataChannel chunking.

**FEC is off.** The stream offers carry no `ulpfec` payload type, and the SFU does not negotiate ULPFEC (mello-sfu 01-SFU.md §6.1). The stage 4 measurement decides if FEC comes back, end to end. Before, the host sent one parity packet per 10 media packets: no viewer received them (the SFU FEC track had no binding), and the parity plus RTP overhead (about 1.13×) was over the 1.08× pacing ceiling, so the AU queue overflowed. The sender and receiver FEC code stays; it runs only when an answer negotiates PT 127.

| Parameter | Value |
|-----------|-------|
| Payload type | 96 (H.264 media) |
| FEC | Off (no `ulpfec` payload type in the offers) |
| Clock rate | 90 kHz |
| Packetization | RFC 6184 mode 1, max payload 1100 bytes |
| Host output | Annex-B access units (SPS/PPS on every IDR, no B-frames) |
| RTCP feedback | `nack`, `nack pli`, `goog-remb`, `transport-cc` |
| Header extension | TWCC (`transport-wide-cc`, id 3) |

**Sender (libmello):** bounded AU queue, per-packet leaky-bucket pacing (fragments of a frame are spread across their wire time, with a two-packet-interval lag allowance), a caching NACK responder whose retransmits drain through a priority RTX queue on the pacing worker (rate-accounted, 512-packet cache with 1 s TTL), PLI/REMB RTCP callbacks. Rust sets pacing ceilings via `PacketSink::set_pacing_kbps`. When an answer negotiates ULPFEC (`ulpfec/90000` PT 127), the sender emits one parity packet per group of 10 consecutive media RTP packets (XOR over pre-TWCC-stamp bytes; parity rides PT 127 on `media_ssrc + 1`). No offer negotiates it now.

**TWCC congestion control:** when negotiated, egress packets carry transport-wide sequence numbers (stamped in the pacer at emit time; retransmits get fresh seqs). The receiver emits TWCC feedback every ~50 ms in the transport-cc draft layout: run length chunks carry a 2-bit symbol and a 13-bit run, small deltas are unsigned 8 bits and large deltas signed 16 bits (250 µs units, never clamped), and the reference time and the first delta use one clock (the first arrival rounded down to 64 ms). One report holds at most about 1.1 kB; the packets that do not fit go in the next report. Pion parses it (mello-sfu `TestPionParsesLibmelloTWCCFeedback`). Before streaming-quality stage 4 the run length chunk had a 1-bit symbol, so Pion rejected about 18 reports per second as `rtcp: packet too short`, and the host parser read the run length chunks of the SFU's feedback as long runs of lost packets. the sender's delay-gradient estimator (GCC-style: accumulated-delay trendline, overuse detector, AIMD + loss cap) produces a send-side target. The pacer runs at the manager ceiling. The estimator target does not set the pacing rate: the estimator updates its target only when an inter-arrival group closes, and under continuous pacing no group closes, so the target stays at its 4 Mbps start value (streaming-quality handoff §12.2). The estimator target is the host's own estimate for the host-to-SFU hop. It is forwarded to Rust as `GCC_TARGET` feedback and shown in the stats. The stream manager does not apply it to the encoder: the SFU sends the viewer-path capacity as REMB.

**Receiver (libmello):** the H.264 receiver, the receive timeline and NACK/PLI/RR/REMB/TWCC (§7.1 to §7.3). A missing packet is NACKed every max(RTT, 10 ms) until its repair deadline (the render time of the first frame after the gap plus one RTT, and not before the adaptive repair window of 300 ms to 1.5 s); complete access units go to the timeline in any order; a PLI goes out only when the timeline needs a key frame. The 3-AU window, the 120 ms stall, the 600 ms hard age and the fixed NACK budget are gone (streaming-quality stage 3). When ULPFEC is negotiated, parity packets (`ssrc == remote_media_ssrc + 1`, PT 127) feed a recovery buffer; `send_nack` tries XOR reconstruction before emitting NACK; eager repair retries on FEC and media arrivals and on idle worker ticks when covered sequences remain missing. Recovered packets are injected outside receiver callbacks (no re-entrancy). Stats: `rx_fec_recovered`, `rx_fec_unrecoverable`.

**Stream session DataChannel:** reliable `control` only (viewer PLI/loss metadata, cursor, ping/pong). There is no unreliable stream-video DataChannel. Host and viewer send a control-channel ping every ~2 s so both sides have a live RTT measurement (`rtt_ms` in telemetry).

Implementation: `libmello/src/transport/rtp_video_sender.cpp`, `rtp_video_receiver_session.cpp`; Rust wrappers in `mello-core/src/stream/rtp_peer.rs`.

---

## 6. Transport

### 6.1 PacketSink Trait

The stream manager sends access units to a `PacketSink`. `SfuSink` is the one implementation; the stream manager tests use a fake sink.

| Sink | Transport | Max viewers | Video path |
|------|-----------|-------------|------------|
| `SfuSink` | One SFU signaling connection + one host RTP track | 100 by current backend/SFU defaults | One encoded stream; SFU relays RTP |

The sink exposes `native_rtp_telemetry()`, `poll_video_feedback()` (PLI/REMB), and `set_pacing_kbps()`. The SFU aggregates viewer REMB through the relay and reports it under the synthetic viewer id `sfu`.

### 6.2 SFU Connection

`SfuConnection` handles the SFU lifecycle: WebSocket signaling (connect, join, negotiate ICE/SDP), **one H.264 RTP video track** (host send / viewer recv), reliable **control** DataChannel only, and event polling. The SFU is a Go service (`mello-sfu`) that forwards RTP without transcoding.

When the RTP video track or control channel closes, send attempts return errors that flow through `video_send_fail_total` telemetry counters.

Stream peers use active ICE-TCP like voice peers: a network that blocks UDP can reach the SFU's passive ICE-TCP port. Stream peers also get the same STUN and TURN servers as the voice peer (`get_ice_servers`, `create_stream_peer`'s `ice_servers`); the probe tools pass none. Spec 10 section 7.6 lists the libjuice limits of that path.

### 6.2.1 Stream membership

The SFU lists the other members of the session in `joined.members`. It then sends `member_joined {user_id, role}` and `member_left {user_id, reason}` to every stream member (mello-sfu 01-SFU.md section 4.1). An SFU older than mello-sfu stage 4d sends them to the host only.

| Side | What the client does |
|---|---|
| Host | `MemberJoined` with role `viewer` and `MemberLeft` go to the `PacketSink` (`on_viewer_joined`, `on_viewer_left`). A join requests no IDR: `member_joined` comes before the viewer's ICE and DTLS are up. |
| Viewer | `ViewerState.members` (`stream::StreamMembers`) starts from `joined.members`. Each `MemberJoined` and `MemberLeft` updates it. A change emits `Event::StreamMembersChanged` with the whole list. |

Rules of the viewer list (`mello-core/src/stream/members.rs`):

- The list holds the other members in join order. Each user is in it once: a reconnect sends `member_joined` again.
- The list holds at most one host. A viewer that joined before the host gets `member_joined {role: host}`; that sets the host. A second host entry replaces the first.
- An SFU that sends no events to viewers leaves the `joined.members` snapshot.

No UI shows the list. The UI viewer count comes from crew state (`viewer_count`), the length of `stream_meta.viewer_ids`: `watch_stream` adds the viewer, and the host calls `stream_viewer_left {crew_id, viewer_id}` on each `MemberLeft` (`report_stream_viewer_left`). The SFU sends `member_left` only for a viewer that left, not for one that reconnected. The e2e state port reports the list as `stream_members` (`client/src/e2e_state.rs`); `StreamWatchingStopped` clears it.

### 6.3 Topology

The SFU is the only stream topology. `start_stream` and `watch_stream` always return `mode: "sfu"` with an SFU endpoint and token.

| Condition | RPC result |
|---|---|
| The server has no SFU signing key, or signing fails | Error `SFU_UNAVAILABLE` (code 14). `start_stream` writes no stream state. |
| The crew metadata has `"sfu_enabled": false` (operator kill switch) | `start_stream` returns `SFU_UNAVAILABLE`. |
| A `watch_stream` session has no SFU endpoint (a record from before the SFU was the only transport) | `SFU_UNAVAILABLE`. |

The client logs the error and reports a stream error. A failed SFU connect, join or control channel on the host, or a failed session creation, stops the native host, retracts the advertised stream (`stop_stream` for the crew of the start) and reports a stream error.

---

## 7. Viewer Pipeline

Streaming-quality stage 3. The viewer runs in libmello, on its own threads.
The command loop only reads stats, follows the pause state and the members,
and keeps the freeze clock (§7.8).

```
RTP ingress → H.264 receiver → receive timeline → decode thread → 3-slot ring → present thread → DComp swap chain
 (worker)     (repair, NACK)    (FrameBuffer,      (HW decode)    (per-slot       (render time,     (waitable object,
                                 VCMTiming)                        textures)       keyed mutex)       keyed mutex)
```

`mello_stream_viewer_attach_peer(view, peer)` connects the view to its
StreamViewer peer. From then on nothing polls access units, feeds packets or
presents: the peer's receive timeline releases frames, the view's decode thread
decodes them, and the view's present thread calls the frame callbacks at each
frame's render time. A new video track of the peer (a renegotiation) brings a
new receiver session and timeline; the decode thread asks the peer for it again.

### 7.1 Native RTP receive

`RtpVideoReceiverSession` (`libmello/src/transport/rtp_video_receiver_session.cpp`)
copies each datagram on the libdatachannel thread into a bounded ingress queue
(2048 packets, 4 MB). One worker thread owns the H.264 receiver, the RTCP state
and every feedback packet. A full ingress queue drops the newest packet only
(the receiver NACKs the gap); it never flushes the queue and never rebuilds the
receiver.

The H.264 receiver (`rtp_h264_receiver.cpp`) repairs and assembles:

| Rule | Value |
|---|---|
| Emit | An access unit goes out as soon as it is complete, also before an older one that still waits for a repair. The timeline puts them in order. |
| Frame id | The unwrapped sequence number of the access unit's last packet. Its reference is the last media packet before its first packet (padding packets are skipped). A key frame (IDR with SPS and PPS) has none. |
| Start of an access unit | The packet before is the previous access unit's last packet (marker bit, or another RTP timestamp). After a lost packet, only a payload that can open an access unit (AUD, SEI, SPS, or the slice at macroblock 0) starts one. |
| NACK | First after 3 ms of reorder grace or 2 newer packets, then every max(RTT, 10 ms). |
| Repair deadline | The render time of the first frame after the gap plus one RTT; never before the last packet of that frame plus one RTT (a large key frame still arriving) and never before the repair window has passed since the gap was seen. |
| Repair window | 1.5 times the slowest repair of the last 10 s, and at least 3 RTT; between 300 ms and 1.5 s. A repair is the time from the arrival of the packet before the gap to the packet that fills it: a NACK repair, a late original, or a packet that came after its deadline. A repair crosses the viewer leg's jitter twice, so a fixed deadline turns each jitter spike into a loss and a key frame. |
| Loss | A packet still missing at its deadline is lost for real: `on_loss` runs once and the access units that needed it are dropped. |
| Key frame | Everything before it is obsolete: no more repair for older gaps. |
| Discontinuity | A gap over 2048 packets is not NACKed: everything before it is lost. |
| Packet memory | 4096 packets or 8 MB of incomplete access units. When the stream is silent for 600 ms, incomplete access units are forgotten (memory only, no key-frame request). |

The RTT is the control channel's smoothed ping RTT. `set_rtt_hint` applies at
once to the NACK interval and the repair deadlines. The timeline gets the
larger of the RTT and the slowest repair of the last 10 s
(`RtpH264Receiver::repair_latency`).

### 7.2 Receive timeline

`VideoReceiveTimeline` (`libmello/src/transport/video_receive_timeline.cpp`, a
C++20 island over the vendored M131 overlay `third_party/webrtc-video-timing`)
is WebRTC's `FrameBuffer` and `VCMTiming` behind a synchronous controller that
replaces upstream's `VideoStreamBufferController`. One per viewer session.

- **Insert** (receiver worker): each complete access unit, in any order. A
  frame decodes when its reference was decoded.
- **Poll** (decode thread): `Release{frame, render_time}`, `Wait{wake}`,
  `KeyframeNeeded` or `Idle`. `wait(wake)` blocks until a frame arrives or the
  wake time.
- **Target delay** = jitter estimate + 95th percentile decode time + 10 ms
  render delay, between 1 ms and 150 ms. The minimum is 1 ms, not 0: a minimum
  of 0 with a maximum under 500 ms switches VCMTiming to low-latency rendering,
  which has no jitter buffer. The delay a frame gets (VCMTiming's current
  delay) moves toward the target at most 100 ms per second, up and down, at
  each released frame. Upstream raises it only when a frame that was not
  repaired decodes late, so it never makes room for a repair and never comes
  down after a lossy period.
- **Room for a repair:** for 10 s after a repaired frame, the jitter delay is
  at least the timeline's RTT (above). Upstream adds the RTT only after 3
  repaired frames and forgets it at each estimator reset, which the staggered
  pair does every 2 s.
- **Jitter estimate:** two upstream `JitterEstimator`s on the same input, each
  restarted every 4 s, 2 s apart; the larger estimate counts ("staggered
  pair"). Upstream alone forgets over about 13 s and does not rise in a 3 s
  jitter spike. The pair rises within a second and returns within 5 s after the
  spike.
- **Late frames:** a late frame is decoded late, never dropped (each H.264 frame
  needs the one before). A delta frame more than 1 s past its render time means
  decode cannot keep up: the timeline drops to the next key frame and asks for
  one instead of decoding a backlog.
- **Minimum delay** (`set_min_delay_us`, stage 7 A/V sync) is applied again
  after a timing reset.

### 7.3 Key-frame requests (PLI)

A PLI goes out only for the timeline's `KeyframeNeeded`, with a 1 s cooldown in
the receiver session. `KeyframeNeeded` comes from:

| Cause | Effect |
|---|---|
| A packet missed its repair deadline (`on_loss`) | One request. The frames before the gap still play; the chain waits at the gap (at least the 300 ms repair window). A loss less than 1 s after a request, while the timeline still waits for that key frame, asks nothing more: the retry covers it. |
| The decoder refused a frame | Delta frames are dropped until a key frame. |
| A delta frame more than 1 s late | As above. |
| The first frame is not a key frame | First request 50 ms after the first frame (reorder room). |

While the timeline waits for a key frame it asks again once per second, and
only while frames arrive. The session sends no PLI while a key frame is arriving
(an access unit that starts with an SPS or IDR slice, last packet under 200 ms
old). A late joiner gets the SFU's cached IDR when its PeerConnection connects
(mello-sfu 01-SFU.md §6.1); the jump to the live sequence numbers is a
discontinuity, so the timeline asks for a fresh IDR at once.

### 7.4 Hardware Decode

`libmello/src/video/video_viewer.cpp` (`ViewerPipeline`). The decoder writes
each picture into one of 3 output slots, a texture per slot, chosen by the
caller:

| Decoder | Slot texture | Path |
|---|---|---|
| NVDEC | R8 with the NV12 layout (luma rows, then chroma rows) | CUDA-D3D11 interop: each slot texture is registered with CUDA once; the decoded surface goes into it with `cuMemcpy2D` on the GPU. No host copy, no upload. The copy runs after `cuvidParseVideoData` returns, not in a parser callback. |
| AMF | NV12 | The AMF surface (a texture array slice) is copied into the slot texture on the GPU, so the surface goes back to AMF's pool at once. |
| OpenH264 | NV12 | I420 to NV12 on the CPU, then `UpdateSubresource` into the slot texture. |
| VideoToolbox (macOS) | CVPixelBuffer | The ring retains the decoder's pixel buffer. |

There is no D3D11VA decoder. It was a stub (the raw access unit as one
bitstream buffer, no DXVA picture parameters) that decoded nothing and was
chosen on Intel before OpenH264; it was removed in stage 3, so an Intel viewer
decodes with OpenH264. A full D3D11VA implementation needs an Intel box to test
on. The order is NVDEC, AMF, OpenH264; `MELLO_SOFT_DECODER=1` forces OpenH264.

The picture size comes from the SPS. A new size makes new slot textures; the old
ones live on while the present thread holds them (stage 4's resolution rungs
need this).

A decode error tells the timeline (`on_decode_error`), which asks for a key
frame.

### 7.5 Decoded-frame ring

Three slots, one per decoder output slot. States: free, decoding, ready (with
its render time), presenting. The decode thread takes a free slot; when none is
free, the oldest ready frame gives its slot up (counted as dropped). A slot the
present thread reads is never the slot of the next decode.

### 7.6 Present clock

One present thread per viewer is the only present clock. It sleeps until the
render time of the next ready frame (1 ms timer resolution while a viewer runs),
shows the newest frame that is due and drops the older ones that are due too
(`frames_dropped_late`). The 16 ms command-loop tick and the 16 ms Slint timer
present nothing.

On Windows the present thread converts the picture into one shared RGBA texture
(`StagingTexture`, compute shader) and calls the native frame callback. The
shared texture has a keyed mutex: libmello writes it under key 0, the presenter
reads it under key 0, so the presenter never reads half a frame. The texture
follows the picture size (a new size gives a new NT handle).

| Decoder output | Shader entry | Source views |
|---|---|---|
| R8, NV12 layout (NVDEC) | `CSMain` | one R8 view; chroma from row `uv_y` |
| `DXGI_FORMAT_NV12` (AMF, OpenH264) | `CSMainNv12` | R8 view of the luma plane, R8G8 view of the chroma plane |

The CPU frame callback (the gate and the tools) gets the RGBA read back on the
present thread. The `ts` of both callbacks is the present time, taken before
the conversion and the readback.

### 7.7 DirectComposition Rendering (Windows)

Video frames bypass Slint's renderer entirely. A separate D3D11 device,
composition swap chain, and DComp visual tree are created when the viewer starts
watching. Slint renders the UI with Skia's CPU rasterizer, so no GPU context
exists when not streaming (see [12-NATIVE-PLATFORM.md §3.1](./12-NATIVE-PLATFORM.md)).

`client/src/dcomp_presenter.rs` has two halves on two threads:

- `VideoSwapChain` presents, on libmello's present thread (installed with
  `mello_core::set_native_presenter`). For each frame it waits on the swap
  chain's frame-latency waitable object (`DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`,
  maximum frame latency 1: at most one frame in the queue), opens the shared
  texture when the handle or size changes, acquires its keyed mutex (key 0),
  copies it into the back buffer, releases the mutex and calls `Present(1, 0)`.
  A new texture size resizes the swap chain.
- `DCompPresenter` stays on the UI thread: the visual tree, the geometry from
  Slint, the pause card and the host's pointer (a child visual, §10). The Slint
  timer only updates the pointer, applies the geometry again after a stream size
  change and reads the present count for the debug fps.

The presenter's D3D11 device is multithread-protected: the pointer upload (UI
thread) and the frame copy (present thread) share its immediate context.

**DComp visual tree:**

```
IDCompositionTarget (bound to the Slint HWND)
  └─ IDCompositionVisual
       ├─ Content: IDXGISwapChain1 (CreateSwapChainForComposition, waitable)
       ├─ Offset: SetOffsetX/Y (physical pixels)
       ├─ Transform: Matrix3x2 scale (stream resolution → card size)
       ├─ Clip: IDCompositionRectangleClip (scroll viewport intersection)
       └─ Child: the host's pointer (§10)
```

**Geometry sync:** The Slint stream card contains a zero-size `geo-tracker` element with properties bound to `media-rect.absolute-position` and dimensions. Slint `changed` handlers fire a `VideoRect.geometry-changed` callback synchronously during every layout pass (scroll, resize, reflow). Rust wires this callback to `DCompPresenter::update_geometry`, which:

1. Multiplies logical pixel coords by `window.scale_factor()` to get physical pixels.
2. Intersects the canvas rect with the scroll container (Flickable) viewport.
3. Calls `SetOffsetX/Y`, `SetTransform2` (scale matrix), `SetClip` (viewport intersection), `Commit`.
4. When fully scrolled out of view, removes swap chain content from the visual (`SetContent(None)`).

Scroll is cheap: only offset + clip + commit, no swap chain resize.

**Swap chain format:** `DXGI_FORMAT_R8G8B8A8_UNORM`, `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`, 2 buffers, `DXGI_ALPHA_MODE_IGNORE`, frame-latency waitable object. Matches libmello's shared texture format. Swap chain is created at stream resolution; a DComp scale transform maps it to the card's display size.

**Lifecycle:** `DCompPresenter` is created when `StreamWatching` fires and installs the native presenter; `StreamWatchingStopped` and `StreamEnded` remove the native presenter (which waits for a present in progress) and drop it. The Slint card's video area is transparent and the `Image` element is hidden (`visible: false`) because the DComp layer renders the actual video. The current visual is composition content above the Slint surface, not a true window underlay; see Known Gaps.

### 7.8 The stream tick (command loop)

`stream_tick` on the command loop (16 ms) does stats, pause state and membership
only:

- the receive-target controller (`ViewerCongestionController`, REMB) and the 2 s
  control-channel ping;
- the pause slot (the host's pause message, §9 viewer mute), the game-audio mute and the SFU events (members, game
  audio packets, disconnect);
- the freeze clock: the frame callbacks count each presented frame
  (`delivered_frames`); a tick with new frames ends a freeze;
- once a second the debug event, the `Stream timeline` log line and the
  `StreamViewerTimeline` event; every 10 s `stream_client_stats` (§12.1).

Nothing on the tick waits on the network: the `stream_client_stats` send and the
host's `stream_viewer_left` RPC run on their own tasks.

---

## 8. Quality Presets and Congestion Control

### 8.1 Presets

| Preset | Resolution | FPS | Bitrate (H.264) |
|--------|-----------|-----|-----------------|
| **Ultra** | 1920×1080 | 60 | 8 Mbps |
| **High** | 1920×1080 | 30 | 4.5 Mbps |
| **Medium** | 1280×720 | 60 | 4 Mbps |
| **Low** | 1280×720 | 30 | 2.5 Mbps |
| **Potato** | 854×480 | 30 | 1.5 Mbps |

Default is Medium. The host can select a preset before starting. The GPU preprocessor downscales capture to the preset's target resolution. Preset `fec_n` fields remain in config for schema compatibility but are unused on the RTP path.

### 8.1.1 Adaptive framerate ladder (Stage 1)

The preset fixes geometry and framerate at stream start; §8.2 then moves only
bitrate inside it. When throughput falls that starves the picture rather than
simplifying it — 720p60 at 1.5 Mbps is ~0.027 bits per pixel per frame, about a
third of what the format needs, which is how a stream reaches single-digit
delivered fps. §1 already commits to the opposite: *favor visible quality loss
over lag or stalling*.

`FramerateLadder` (`stream/ladder.rs`) owns the encoder's framerate target and
trades in **bits per pixel per frame**, `bitrate / (w × h × fps)` — preset-
agnostic, so one threshold pair covers Ultra through Potato:

| Transition | Condition | Dwell |
|---|---|---|
| full → half rate | bpp at full rate **< 0.055** | 2 s |
| half → full rate | bpp at full rate **> 0.075** | 15 s |

10 s cooldown between switches, and no rung below 30fps. Both directions are
judged **at full framerate**: dropping to 30fps instantly doubles the measured
bpp, so judging at the current rung would argue for climbing straight back into
the starvation just escaped.

Reduction is by **decimation before GPU colour conversion**
(`VideoPipeline::set_output_fps`), so convert *and* encode cost fall and no
capture backend needs reconfiguring. The encoder is retargeted too
(`Encoder::set_framerate`) because rate control budgets bits per frame from the
framerate — an encoder still told 60 while fed 30 hands out half the bits each
frame deserves. Each switch forces an IDR.

Stage 2 (geometry rungs: 540p/480p/360p) needs viewer-side work — mid-stream SPS
geometry change and a `DCompPresenter` swap-chain resize — and is not built.
See `plans/STREAM-CAPTURE-QUALITY.md`.

### 8.2 REMB congestion control

**Viewer (`ViewerCongestionController`):** Samples native RTP receiver stats every 500 ms. Severe loss (>5%), incomplete AUs, or gate pressure step the receive target down 25%; mild loss (2–5%) or jitter >20 ms steps down 15%; ten consecutive good samples increase by max(100 kbps, 5%). Emits REMB at significant changes or every 2 s heartbeat.

**Host (`StreamManager`):** Applies the minimum fresh REMB target across active viewers (3 s stale expiry). Encoder bitrate changes are coalesced: a target within 5 % of the current bitrate changes nothing (the dead band); a decrease over the dead band applies immediately; an increase waits until 250 ms after the last change, is rate-limited to 5%/s, and applies only over the dead band (the last step to the preset ceiling can be smaller). REMB is polled every 10 ms, so before this the encoder got up to 100 changes per second during a ramp. Bitrate changes trigger encoder reconfigure (+ IDR on down-steps > 25%). When every estimate is stale or missing (lost RTCP), the host **holds** the current target — restoring max on transient REMB loss ramps the host back into the congestion that caused the silence. A last-viewer-leave is an explicit signal and does restore the configured ceiling for the next viewer. Pacing target includes RTP header headroom via `calc_stream_pacing_target_kbps`.

In SFU mode all viewers share one encoded stream. The SFU terminates TWCC per hop: it generates TWCC feedback to the host (host→SFU leg) and stamps per-leg transport-wide sequences toward viewers. It runs no estimator and no pacer of its own: the minimum of the fresh viewer REMB values goes upstream as REMB, on arrival and on the ~1 s tick. The SFU caches the last complete IDR access unit and sends it to a viewer when the viewer's PeerConnection connects.

---

## 9. Audio Streaming

Game audio is wired end-to-end on Windows and macOS:

| Parameter | Value |
|-----------|-------|
| Capture (Windows) | WASAPI **process** loopback (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`), endpoint loopback as fallback |
| Capture (macOS) | ScreenCaptureKit, on the SCStream that already captures video |
| Encode | Opus stereo 48 kHz, 20 ms frames, `OPUS_APPLICATION_AUDIO`, ~96 kbps |
| RTP payload type | 111 (Opus), same as voice but on a separate stream peer connection |
| Host SDP | sendonly audio m-line alongside sendonly H.264 |
| Viewer SDP | recvonly audio m-line alongside recvonly H.264 |
| Viewer RTP receive | `RtcpReceivingSession` on the recvonly audio track before `onMessage` (libdatachannel media-receiver pattern) |
| SFU relay | 1 host ingress → N viewer fan-out (`fanOutAudioRTP`), queue depth 32 |
| Viewer playout | `mello_stream_feed_audio_packet` → Opus decode → `create_audio_playback()` (stereo: WASAPI on Windows, CoreAudio on macOS), on the output device chosen for voice (`mello_set_audio_output`; the default device when none is set or the chosen one fails) |
| Viewer mute | `mello_stream_viewer_set_audio_muted(view, true)` closes the output device, which drops the queued audio, and drops packets until unmute. Unmute opens the device at the next packet with a fresh decoder. The viewer tick (`client/streaming.rs`) mutes when the pause slot turns paused and unmutes when it turns live, once per change (`pause_audio_mute_change`). A viewer that joins a paused stream gets the pause state from the host and mutes on its first tick |

Host path: `mello_stream_start_audio` → `MelloAudioPacketCallback` → `StreamManager::handle_audio` → `PacketSink::send_audio` → `mello_peer_send_audio`. Viewer path: `AudioRtp` events (complete RTP packets from `mello_peer_set_audio_rtp_callback`; `feed_viewer_audio_rtp` strips the header) → the decoder. Stream viewer receive is wired on the offered recvonly audio track (not voice `onTrack`); if `onTrack` fires for the same `mid`, callbacks move to that track instance.

### 9.0 Capture scope (Windows)

Endpoint loopback captures the entire system mix. That includes Mello's own
voice playback, so the viewer hears *their own voice* echoed back out of the
streamer's speakers, plus notifications and any other application. Windows 10
2004+ can scope loopback to a process tree, which fixes all three at once:

| Streaming | Scope | Target |
|---|---|---|
| a game (process capture) | `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` | the game's pid |
| a monitor or window | `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` | **our own** pid |

The exclude case matters as much as the include case: a desktop stream should
still carry the desktop's sound, just not the voice chat mixed into it.

Activation goes through `ActivateAudioInterfaceAsync` against
`VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`, which is asynchronous, so
`WasapiLoopbackCapture` waits (bounded, 3 s) on a completion handler rather than
letting stream start hang. The virtual device has **no mix format to query**, so
the format is chosen rather than negotiated: 48 kHz stereo int16, matching the
internal contract exactly and removing the resample path.

On older Windows, activation fails and capture falls back to endpoint loopback —
logged at WARN naming the consequence, because "you can hear yourself" is the
symptom users report and it must be attributable.

### 9.1 Inbound audio framing

The viewer gets complete RTP packets from `mello_peer_set_audio_rtp_callback`
(`SfuEvent::AudioRtp`). `client/stream_ffi.rs::feed_viewer_audio_rtp` strips
the RTP header (`rtp_payload`: fixed header, CSRC list, header extension and
padding) and feeds the Opus payload to the decoder. libmello adds no framing
of its own.

### 9.2 macOS capture (ScreenCaptureKit)

macOS has no loopback device. ScreenCaptureKit delivers system audio on the
same `SCStream` that already captures video, so `StreamAudioHostPipeline` opens
no source of its own: `mello.cpp` registers a `CaptureSource::set_audio_callback`
that pushes into `feed_float_pcm`.

Four constraints, none of which are visible from the call site:

- **`capturesAudio` is always enabled when hosting.** `SCStream` fixes it at
  *creation*, but `mello_stream_start_audio()` runs after
  `mello_stream_start_host()`. Turning it on later is impossible, so it is
  always on and the audio callback decides whether samples go anywhere.
- **SCK delivers planar (non-interleaved) float.** Audio must be read through
  `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer`, one `AudioBuffer`
  per channel. Reading the block buffer directly as interleaved — the shape
  `CMBlockBufferGetDataPointer` invites — produces garbled audio with no error.
  Formats that are not 32-bit float are rejected and logged, not reinterpreted.
- **48 kHz only.** There is no resampler in libmello. SCK is configured for
  48 kHz stereo; audio arriving at another rate is dropped with a warning rather
  than encoded at the wrong pitch. Channel count *is* adapted — mono is
  duplicated to both sides, more than two channels keeps the front pair.
- **Detach before teardown.** The capture callback holds a raw pointer to the
  pipeline and fires on SCK's own audio queue, so both `mello_stream_stop_audio`
  and `mello_stream_stop_host` clear the callback before dropping the pipeline
  (`stream_audio_teardown`).

Covered by `libmello/tests/test_stream_audio_pipeline.cpp`, which decodes the
emitted Opus and asserts on signal level per channel. Packet counts alone do not
discriminate: the output buffer size drives them even when the conversion fills
only part of each frame.

**Windows smoke test (Aug 2026):** host `audio_out_hz≈50`, viewer `audio_fed_hz≈50`, `rx_audio_packets` climbing, `viewer playout started`. Same-machine host+viewer may loop back via WASAPI loopback — use headphones or separate machines for listen tests.

---

## 10. Cursor Streaming

Each capture backend says who puts the pointer in front of the viewer
(`CaptureSource::cursor_mode`):

| Backend | Mode | What happens |
|---|---|---|
| WGC (window and monitor) | in frame | `IsCursorCaptureEnabled(true)`: WGC draws the pointer into the frames itself. |
| DXGI desktop duplication | composite | The desktop image has no pointer. The preprocessor draws the duplication pointer shape (colour, monochrome and masked colour, as straight-alpha BGRA) into a copy of the frame before the colour conversion. The capture's own texture is never written. |
| Game capture hook | packet | The back buffer never has the system pointer, and drawing it in would double the pointer a game draws itself. The viewer draws it from cursor packets. |

**Cursor packets** (hook only). The host manager polls
`mello_stream_get_cursor_packet` 30 times a second and sends what it returns on
the reliable control channel as `[0x04, 0x02, x i32, y i32, visible, has_shape,
(w u16, h u16, RGBA)]` (`mello-core/src/stream/cursor.rs`, libmello
`cursor.cpp`). libmello returns a packet only on a change. The position is the
top-left of the shape in encode pixels; the shape is scaled as the frame is, at
most 128 a side. A shape travels only when it changes; the manager keeps the
last shape packet and the last packet and sends both to a viewer that joins.
The hook takes the system pointer (`GetCursorInfo`) when it is over the game's
window and the game is the window under it, and maps the window's client area
onto the back buffer size. When the stream moves to a backend with another
mode, one hidden packet tells the viewer to stop drawing.

**Viewer.** The control-channel callback parses the packet into the watched
stream's pointer (one presenter, one pointer). The DComp presenter draws it as
a child visual of the video visual, so the video's scale and clip apply and the
offset is in stream pixels. A new shape is uploaded once into a small
premultiplied-alpha composition swap chain; a move only sets the offset. The
pointer hides with the video for the pause card.

---

## 11. Stream Lifecycle

### Host start

1. User picks a capture source (game process, window or monitor) in the UI. For a game, the client takes `exe` and `igdb_id` from the `stream_games` row of that pid.
2. `mello-core` calls `start_stream` RPC to Nakama → gets session ID, mode, SFU endpoint, and the `capture` block. For a process target, `hook_allowed_for_game` turns the block and `igdb_id` into `allow_hook` (§3.1.1).
3. `start_host` creates the libmello video pipeline (capture + preprocess + encoder) and sets up mpsc channels for encoded packets.
4. `create_stream_session` creates a `StreamManager` with the appropriate `PacketSink` and spawns its async `run` loop. The `StreamSession` keeps the crew id of the start (`PendingStreamStart.crew_id`).
5. `start_stream` advertises the stream before the host has started. Every start failure after it (encoder check, native start, SFU connect, join, transport, session creation) calls `stop_stream` for that crew, so no viewer sees a stream that does not exist.

### Nakama reconnect

The backend removes the stream records when the host's realtime socket ends (`OnSessionEnd` → `StreamCleanupUser`). The SFU session and the encoder keep running. After the reconnect, `resync_after_reconnect` calls `start_stream` with `crew_id` and `resume_session_id` (the live session). The backend restores the same session (specs/04-BACKEND.md §5). When it cannot, no viewer can find the stream, so the host ends it and reports a stream error. A start still on its thread during the reconnect is not re-advertised.

### Viewer start

1. Viewer discovers stream via crew state (Nakama).
2. For SFU: connects to the SFU endpoint, joins the session, negotiates WebRTC.
3. Uses the encode resolution and bitrate returned by `watch_stream` (falling back to crew-state/default values only for older responses).
4. Creates the decoder pipeline at the correct resolution (`mello_stream_start_viewer`).
5. Attaches the viewer peer (`mello_stream_viewer_attach_peer`): libmello decodes and presents on its own threads from then on (§7).
6. Creates `DCompPresenter` with the stream resolution and parent HWND (Windows) and installs its swap chain as the native presenter.
7. `stream_tick` (16 ms) reads stats, follows the pause state and the members, and keeps the freeze clock; `ViewerCongestionController` emits REMB from native stats.
8. `VideoRect.geometry-changed` callback keeps the DComp visual in sync with the Slint card layout.

### Teardown

Host: signal `StreamSession::stop_and_wait` so the manager drains sinks before peer teardown. `stop_stream` goes to the crew in `StreamSession.crew_id`, not to the crew the UI shows. The backend accepts it only from the host. Viewer: stop pipeline, release GPU resources, leave the SFU session. The host also stops on user request and when the captured process quits (`target_exited`); both run the same teardown and `stop_stream` RPC.

---

## 12. Telemetry and Diagnostics

### Host-side (per second)

`Stream manager diag`: `video_in_hz`, `audio_in_hz`, `audio_out_hz`, `coalesced_hz`, `recovery_mode`, `keyframe_req_*_total`, `send_fail_video_delta`, `send_fail_audio_delta`, queue lengths/max, `bitrate_kbps`.

`Stream RTP pacing`: `target_kbps`, `out_kbps`, `tx_bytes_total` (from `PacketSink::pacing_telemetry`).

`host_probe_tick` (stream-host tool): `tx_aus_sent`, `tx_aus_dropped`, `tx_rtp_packets`, `tx_rtp_bytes`, `tx_pacing_target_kbps`, `tx_pli`, `tx_remb`, `video_open`, `control_open`, `rtt_ms`.

Encoder periodic (every 300 frames): `convert_ms`, `encode_ms`, `eq_depth`, `eq_drops`.

### Viewer-side (per second)

`Stream timeline` (client, once a second; also the `StreamViewerTimeline`
event): `target_ms` (jitter + p95 decode + render delay), `jitter_ms`,
`decode_p95_ms`, `released`, `late` (released after the decode deadline),
`dropped_late` (never shown), `keyframe_requests`, `losses`, `waiting_key`,
`decoder`, `decode_fps`, `present_fps`, `tick_hz`, `rtt_ms`. The values come
from `mello_stream_viewer_get_stats` (the stage 3 fields of `MelloStreamStats`).

`viewer_probe_tick` (probe): `dec_fps`, `native_fps`, `present_hz`, `au_received_hz` (frames the timeline released), `au_fed_hz` (frames decoded), `decode_queue_depth`, `decode_stall_ms`, `rtt_ms`, `rx_ingress_pps`, `rx_ingress_kbps`, `rx_missing_hz`, `rx_repaired_hz`, `rx_nacks_hz`, `rx_pli_hz`, `rx_jitter`, `rx_gated` (waiting for a key frame), `rx_receive_target_bps`.

`viewer_probe_native_rtp`: `rx_complete`, `rx_emitted`, `rx_incomplete`, `gate_dropped`, `buffered_aus`, `receive_target_bps`.

### 12.1 Remote diagnostics (`stream_client_stats`)

Everything above lands in the *local* log. Field failures happen on machines we
have no access to, so host and viewer additionally report to the SFU every 10 s
over signaling (`stream_client_stats`, one per 5 s per peer, 2048-byte cap —
`mello-sfu/01-SFU.md §5.3`).

**Host:** `src`, `gpu`, `enc`, `enc_tier`, `cap_backend`, `cap_fps`,
`cap_idle_ms`, `enc_fps`, `enc_ms`, `enc_ms_avg`, `enc_submit_ms`,
`enc_wait_ms`, `enc_lock_ms`, `conv_ms`, `eq_depth`, `eq_drops`, `br_target`,
`br_actual`, `pace_kbps`, `w`, `h`, `fps_cfg`, `fps_out`, `recovery`, `vfail`,
`vq`, `aud_in`, `aud_out`, `aud_fail`.

**Viewer:** `freeze_n`, `freeze_ms`, `present_fps`, `rtt_ms`, `rx_complete`,
`rx_incomplete`, `rx_missing`, `rx_repaired`, `rx_gated`, `rx_nacks`, `rx_pli`,
`rx_jitter`, and the receive timeline (stage 3): `tgt_ms` (target delay),
`jit_ms` (jitter estimate), `dec_p95` (p95 decode ms), `rel` (frames released),
`late` (released after the decode deadline), `drop_late` (never shown),
`kf_req` (key-frame requests), `dec` (decoder name), `dec_fps` (decode rate over
the report interval). `bg_drops` (the backlog guard), `rx_fec_ok` and
`rx_fec_fail` are gone.

Four of these carry more weight than the rest:

- **`src`.** What is actually being captured (`process pid=1234`,
  `monitor index=0`). A stream of the wrong source and a stream of nothing look
  identical without it, and the UI label is not evidence.
- **`enc_submit_ms` / `enc_wait_ms` / `enc_lock_ms`.** A single fused encode
  time cannot separate a busy encoder block (another NVENC session competing —
  ShadowPlay, OBS) from a stalled bitstream readback (memory bandwidth). Those
  have different causes and different fixes. `enc_ms_avg` is a rolling mean
  because per-frame encode time swings by an order of magnitude.
- **`enc_tier`.** Whether the adaptive cost downgrade (§3.4) actually fired.
  Without it the mechanism is unobservable in the field.


- **`cap_fps` vs `enc_fps`.** Capture arrivals are counted before any encode
  work, so a stalled capture and a stalled encoder are finally distinguishable.
  `video_in_hz` alone conflates them and cannot answer which one failed.
- **`freeze_n` / `freeze_ms`.** A freeze is a present gap over 150 ms, counted
  once per stall and extended rather than re-counted per tick. Every other
  viewer metric is a proxy; this is the thing the user actually experiences.
  A present counts as a frame only when a frame callback delivered a new
  frame (`delivered_frames`); a present of an unchanged frame does not end a
  gap and does not count in `present_fps`. Gaps while the host is paused or
  idle do not count. Idle: the pipeline re-encodes the last picture at
  2 fps (500 ms gaps) because the capture delivers nothing new. The host
  reads `idle_repeat_frames` on the 1 s manager tick and sends
  `[0x04, 0x04, idle]` on the reliable control channel on each change, and
  to a viewer that joins while idle. The flag arrives up to about 1 s after
  the keepalive starts, so the first keepalive gap of a static scene can
  still count as one freeze.

Payload keys are abbreviated to stay inside the cap as the field set grows; a
unit test sizes the worst-case host payload against it.

DComp presenter diagnostics:

- `ui_render_fps` (DComp present cadence)
- `presented_frames` (total frames presented to swap chain)
- native surface descriptor cadence + sequence gaps
- `DComp present failed` error logs (OpenSharedResource1, CopyResource, Present failures)
- geometry-changed callback frequency (implicit via scroll/resize tracking)
- explicit fatal init error logs that trigger clean `StopWatching`

### Probe tools

| Tool | Purpose |
|------|---------|
| `tools/sfu-stream-viewer-probe` | Standalone viewer; minifb window or `--native-metrics` headless RTP telemetry |
| `scripts/run-stream-host.ps1` | Launch host probe (release, Nakama `start_stream`) |
| `scripts/run-stream-viewer.ps1` | Launch viewer probe; `-NativeMetrics` for headless soak |
| `scripts/coalesce_stream_timeline.py` | Merges host + viewer + SFU logs into a single timeline. Reads the host tick line with `video_open` (and the older `media_open`) |
| `tools/stream-gate` (`scripts/stream-gate.sh`) | The streaming quality gate: the reference clip (`MELLO_CAPTURE_REFERENCE`) through the real host, an impairment shim, the real SFU in test mode and the real viewer, on seven profiles; VMAF, PSNR, SSIM, delivery, glass-to-glass and wire metrics against the frozen baseline (`benchmarks/baselines/stream/`). CPU lane (OpenH264, `MELLO_SOFT_ENCODER=1`, `MELLO_SOFT_DECODER=1`) and hardware lane. `TESTING.md` "Streaming quality gate" |
| `mello-sfu/tools/stream-soak` | SFU relay soak: a Pion host sends the real H.264 fixture (the gate's 720p60 clip, 2 s, FU-A) to N viewers; `--sim-drop-pct` and `--sim-jitter-ms` impair the host leg |

---

## 13. Key Files

| Area | Path |
|------|------|
| **Rust stream module** | `mello-core/src/stream/` |
| Stream manager | `mello-core/src/stream/manager.rs` |
| PacketSink trait | `mello-core/src/stream/sink.rs` |
| SFU sink (RTP) | `mello-core/src/stream/sink_sfu.rs` |
| RTP peer FFI | `mello-core/src/stream/rtp_peer.rs` |
| Viewer congestion (REMB) | `mello-core/src/stream/congestion.rs` |
| Quality presets + config | `mello-core/src/stream/config.rs` |
| Pacing telemetry | `mello-core/src/stream/pacer.rs` |
| Host session setup | `mello-core/src/stream/host.rs` |
| Viewer tick loop (stats, pause, members) | `mello-core/src/client/streaming.rs` |
| Viewer attach, stats, native presenter hook, REMB | `mello-core/src/client/stream_ffi.rs` |
| SFU connection | `mello-core/src/transport/sfu_connection.rs` |
| DComp presenter (Windows) | `client/src/dcomp_presenter.rs` |
| Client render loop + metrics | `client/src/main.rs` |
| Slint stream card UI + geo-tracker | `client/ui/panels/active_streams_panel.slint` |
| VideoRect global (geometry callback) | `client/ui/types.slint` |
| CrewFeed (Flickable viewport source) | `client/ui/panels/crew_feed.slint` |
| **C++ video pipeline** | `libmello/src/video/video_pipeline.cpp` |
| DXGI capture | `libmello/src/video/capture_dxgi.cpp` |
| WGC capture | `libmello/src/video/capture_wgc.cpp` |
| Process capture + hot-swap | `libmello/src/video/capture_process.cpp` |
| GPU preprocessor | `libmello/src/video/video_preprocessor.cpp` |
| NVENC encoder | `libmello/src/video/encoder_nvenc.cpp` |
| Encoder factory | `libmello/src/video/encoder_factory.cpp` |
| Decoder factory | `libmello/src/video/decoder_factory.cpp` |
| NVDEC decoder | `libmello/src/video/decoder_nvdec.cpp` |
| H.264 receiver (repair, assembly) | `libmello/src/transport/rtp_h264_receiver.cpp` |
| Receive timeline (FrameBuffer, VCMTiming) | `libmello/src/transport/video_receive_timeline.cpp` |
| Viewer pipeline (decode thread, ring, present thread) | `libmello/src/video/video_viewer.cpp` |
| Converter (keyed-mutex shared RGBA) | `libmello/src/video/staging_texture.cpp` |
| C API (streaming) | `libmello/src/mello.cpp` (search `mello_stream_`) |
| **Probe tools** | |
| Stream host probe | `tools/stream-host/src/main.rs` |
| Viewer probe | `tools/sfu-stream-viewer-probe/src/main.rs` |
| Timeline script | `scripts/coalesce_stream_timeline.py` |

---

## 14. Current State and Known Gaps

**What works well:** Process-aware capture with hot-swap, deferred start, DXGI adaptive throttle, GPU preprocessing, async NVENC, mutex-guarded decoded ring, proper IDR detection, H.264 RTP transport through the SFU, REMB congestion control, native RTCP recovery, SFU control-only DataChannel, probe tooling with native RTP telemetry, receive timeline (WebRTC FrameBuffer and VCMTiming) with a present thread at each frame's render time, DComp rendering with NT shared handle import (Windows RGBA8 path), callback-driven geometry sync.

**Known gaps and future work:**

| Gap | Impact | Effort |
|-----|--------|--------|
| ~~WGC has no frame throttling~~ **Fixed (v0.4)** — accumulator throttle delivers exactly target_fps | — | — |
| AMF/QSV encoders less tested | No smooth experience for AMD/Intel GPU users | Medium |
| Ladder Stage 2 (geometry rungs) not implemented | Below ~1 Mbps even 30fps 720p starves; needs viewer SPS-change + DComp resize | Medium |
| ~~Game audio not wired~~ **Fixed** — system audio capture (WASAPI loopback / ScreenCaptureKit) → Opus → SFU relay → viewer playout | — | — |
| Input passthrough not implemented | No remote control | Large |
| DComp visual uses overlay, not true underlay (`WS_EX_NOREDIRECTIONBITMAP` not set) | Video composites on top of Slint content; stream card badges moved to bottom bar as workaround | Medium |
| Adapter/device mismatch diagnostics are log-based only | Better in-UI error reasons still needed | Small |
| End-to-end certification matrix pending | Performance targets not yet certified across LAN/loss/multi-viewer gates | Medium |
| macOS viewer has no DComp equivalent | macOS needs its own compositor path (Core Animation layer); the present thread hands RGBA to the Slint timer | Medium |
| No D3D11VA decoder (removed in stage 3) | Intel viewers decode with OpenH264 on the CPU | Large (needs an Intel box) |
| AMF decode slot copy untested on AMD | The array-slice copy follows the AMF samples; not run on an AMD GPU | Small |
| macOS VideoToolbox session churn | Decode FPS drops on SPS/PPS change | Small |

---

## 15. Validation Playbook (Windows)

Use this checklist to validate the DComp rendering path after stream-related changes.

### 15.1 Pre-conditions

- Host and viewer run in release mode.
- Test both 720p60 and 1080p60 scenarios.
- Keep game scene representative (motion + static UI content).

### 15.2 Run commands

Host:

- `./scripts/run-stream-host.ps1 -CrewId "<crew-id>"`

Viewer:

- `./scripts/run-stream-viewer.ps1 -Session auto` (reads the latest session from the host log)

Optional local loopback smoke:

- `cargo test -p mello-sys --test video_pipeline host_to_viewer_loopback -- --nocapture`

### 15.3 Acceptance gates

- `dbg_stream_ui_render_fps` tracks near source cadence (target: near 60 on stable 60fps source).
- `DComp present failed` error logs stay at zero during steady state.
- Scrolling the feed: video moves perfectly with the card, no stutter, no bleed outside the scroll container.
- Resizing the window: video scales with the card, no crash, no black frames.
- Scrolling the card fully out of view: DComp visual is hidden (no overlap with surrounding content).
- DPI change (drag between monitors): video repositions correctly, no crash.
- watch-stream init failures surface as explicit UI/log errors and stop watching cleanly.
- No video D3D11 device exists when not streaming.
