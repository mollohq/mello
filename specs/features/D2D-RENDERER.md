# D2D-RENDERER

> **Component:** Windows client, the Slint renderer
> **Status:** shipped (main, [mollohq/mello#199](https://github.com/mollohq/mello/pull/199), 2026-10-31)
> **Depends on:** `specs/12-NATIVE-PLATFORM.md` section 3.1 (which renderer starts)
> **Origin:** m3llo-fx-lab, `D2D-RENDERER.md` (design and plan) and `WINDOWS.md`
> section 3.6 (measurements). This spec is authoritative. The lab notes are history.

On Windows, mello draws its window on the GPU with its own Slint renderer. The
renderer uses Direct2D on D3D11, and DirectWrite for the glyphs. Its name is `d2d`.

The reason is the RAM rule: under 100 MB private working set (Task Manager
"Memory") while idle in a voice channel. Every Slint GPU renderer measured 133 to
154 MB in mello. `d2d` measured 54 MB, 5 MB over the CPU renderer (section 8).

## 1. What is built

| Part | File | Note |
|---|---|---|
| Renderer entry | `renderer-d2d/src/lib.rs` | `D2DRenderer`: `RendererSealed`, frame loop, idle trim, `probe()`. |
| Device | `renderer-d2d/src/device.rs` | One D3D11, Direct2D and DirectWrite device per thread. |
| Surface | `renderer-d2d/src/surface.rs` | One swapchain and device context per window. Read-back for `take_snapshot`. |
| Drawing | `renderer-d2d/src/itemrenderer.rs` | `ItemRenderer` and `GlyphRenderer`. Brush cache, time per item kind. |
| Text | `renderer-d2d/src/text.rs` | Baseline snap, rendering parameters (section 4). |
| Font faces | `renderer-d2d/src/fonts.rs` | `IDWriteFontFace` per font blob. 32 faces. |
| Images | `renderer-d2d/src/images.rs` | `BitmapCache`, shared by image and size. 32 MB. |
| Frame stats | `renderer-d2d/src/stats.rs` | `FX_D2D_STATS=1` (section 7). |
| Backend hook | `third_party/i-slint-backend-winit/renderer/d2d.rs` | Creates the winit window, gives the HWND to the renderer. |
| Hook edits | `third_party/i-slint-backend-winit/lib.rs`, `Cargo.toml` | Feature `renderer-d2d`, the `mod`, the `"d2d"` arm. Marked `mello:`. |
| Selection | `client/src/renderer_choice.rs` | `d2d` or `skia-software`. The rules are in spec 12 section 3.1. |
| Startup | `client/src/updater/startup_update.rs` | `configure_slint_platform` calls `renderer_choice::choose`. |
| Perf runs | `scripts/perf/voice-idle-memory.ts`, `scripts/perf/reveal-frame-cost.ts` | Driver runs on a release build (section 9). Not in the e2e suite. |

The crate is a workspace member. Its code is `#![cfg(target_os = "windows")]`. Its
dependencies are under `[target.'cfg(target_os = "windows")'.dependencies]`, so
the macOS build does not compile the `windows` crate.

## 2. Device and surface

- One `ID3D11Device` per thread: BGRA support, feature level 11.1 or 11.0. The
  Direct2D factory, the Direct2D device and an offscreen device context come from it.
- The device is created at the first window, or by `probe()` at startup. It is
  dropped when the last window suspends (`release_if_unused`).
- Each window has a flip-sequential swapchain on its HWND: two buffers,
  `B8G8R8A8_UNORM`, alpha ignored.
- Each frame: wrap the back buffer in a target bitmap, `BeginDraw`, draw, `EndDraw`,
  drop the target, `Present1` with vsync and the changed rectangle (section 3.1).
- Resize: drop the target, `ResizeBuffers`. The next two frames draw the whole window.
- The device context DPI stays 96. The renderer applies the scale factor in its
  transform. All units are physical pixels.
- `suspend()` drops the surface, the context and every GPU cache. Font faces stay:
  they belong to DirectWrite, not to the device.
- A lost device (`DXGI_ERROR_DEVICE_REMOVED`, `DXGI_ERROR_DEVICE_RESET`,
  `D2DERR_RECREATE_TARGET`) drops the surface, the shared device and every GPU cache,
  and asks for a redraw. The next frame creates them again on the stored window
  handle. Without the redraw request, a still window shows nothing (see-through)
  until something changes. The rebuild costs about 280 ms, once.
- The stream presenter (`dcomp_presenter.rs`) has its own D3D11 device. Its
  DirectComposition target is `topmost`, so the video composes over the swapchain.
  The HUD overlay has its own window and its own device. These devices stay separate
  (section 11, "Decided").

## 3. Drawing

| Slint | Direct2D | Note |
|---|---|---|
| `draw_rectangle`, `draw_window_background` | `FillRectangle`, `FillRoundedRectangle` | Solid, linear and radial brushes from the brush cache. |
| `draw_border_rectangle` | Fill, then `DrawRoundedRectangle` inset by half the border | Per-corner radii: `ID2D1PathGeometry`. |
| Conic gradient | Bitmap brush, rasterized on the CPU | Direct2D has no conic brush. Same mapping as the software renderer. |
| `draw_image` | `DrawBitmap` | Bitmap from `BitmapCache`. Nine-slice: nine draws. Tiling: bitmap brush. |
| `colorize` | `FillOpacityMask` | The bitmap is the mask, the brush is the fill. |
| `draw_text`, `draw_text_input` | Core's shared parley layout, then `GlyphRenderer` | Layout, wrapping, cursor and selection are in Slint core. |
| Glyph run | `DrawGlyphRun` | Zero advances, parley positions as offsets. COLR fonts: `TranslateColorGlyphRun`. |
| Stroked text | `GetGlyphRunOutline`, `DrawGeometry` | |
| `draw_path` | `ID2D1PathGeometry`, `FillGeometry`, `DrawGeometry` | Fill rule, cap and join from the item. The geometry is cached per item until the path, fit or fill rule changes. |
| `draw_box_shadow` | Shadow effect (`CLSID_D2D1Shadow`) | Cached per item in Slint's `BoxShadowCache`. |
| Rectangular clip | `PushAxisAlignedClip` | |
| Rounded clip | `PushLayer` with a geometric mask | The layer bounds are the content bounds. |
| Opacity | Per-item alpha, or `PushLayer` with opacity | `PushLayer` only when `Opacity::need_layer` says that the children overlap. |
| Transforms, `save_state`, `restore_state` | A `Matrix3x2` stack, `SetTransform` | The state holds the transform, the alpha and the clip depth. |
| `draw_cached_pixmap`, `draw_image_direct` | Bitmap from RGBA, `DrawBitmap` | |
| `take_snapshot` | Read-back of the next frame | The lab's `--capture` uses it. |

Text and images snap to whole pixels.

Not drawn:

- Inset shadows. mello uses none.
- `cache-rendering-hint` layers draw their children directly, with no cache. mello
  uses none.

### 3.1 Partial rendering

Slint's partial renderer (`i_slint_core::partial_renderer`) decides what to draw.
Only the items that changed, and the area they covered, are drawn again.

1. The flip-sequential swapchain keeps its two buffers. The back buffer holds the
   frame from two presents ago (buffer age 2). After a create or a resize, its
   content is undefined (buffer age 0), and the frame draws the whole window.
2. The region to draw is this frame's dirty region plus the previous frame's.
3. The renderer clips the frame to the bounding box of that region, rounded out to
   whole pixels, without antialiasing. A soft edge would blend over the old content.
4. The partial renderer gets the same box as its dirty region. If it kept its
   separate rectangles, an item between them would be cleared and not drawn.
5. The clear respects the clip. `Present1` names the box as the changed rectangle.

`FX_D2D_FULL_REDRAW=1` draws every frame whole, for A/B runs. The renderer reports
its transform in logical pixels (`current_transform`), which the partial renderer
needs to place items. The lab's capture frames are identical with and without
partial rendering, at scale factors 1.0 and 1.5.

## 4. Text

Slint lays out all text in core (shared parley). The renderer only rasterizes the
glyph runs.

- Antialias mode: grayscale. ClearType needs an opaque target, and Skia draws
  grayscale too.
- The glyph baseline snaps to a whole pixel, as Skia does. Without the snap, small
  text is heavier and softer than Skia's.
- The system's DirectWrite rendering parameters stay. Every custom gamma and
  contrast measured further from Skia.
- Font faces come from the font blobs that parley gives the renderer, through an
  in-memory font loader (`IDWriteFactory5::CreateInMemoryFontFileLoader`). The key is
  the blob, the face index and the synthesis (fake bold, oblique).

Measured against `skia-software` on the lab's capture frames:

| Text setting | Mean difference (of 255) | Pixels off by more than 8 |
|---|---|---|
| No snap, system parameters | 0.338 | 0.75 % |
| Baseline snap, system parameters (shipped) | 0.202 | 0.48 % |

## 5. Memory rules

The renderer exists for its memory number. These are design rules. Keep them.

1. **No system-memory copy.** Bitmaps live in the D3D11 device. No CPU copy stays
   after the upload.
2. **Every GPU cache has a bound or an owner.**

   | Cache | Bound |
   |---|---|
   | Direct2D texture memory | 24 MB (`SetMaximumTextureMemory`) |
   | `BitmapCache` | 32 MB. Over it, bitmaps that no item holds go first, oldest first. |
   | Font faces | 32. Cleared when full. |
   | Solid brushes, gradient stops | 256 and 128. When full, entries that the last two frames did not use go. Twice the bound clears all. |
   | Path geometries | Owned by the item. Cleared at a scale change, by `free_graphics_resources` and on a lost device (a geometry belongs to the factory). |
   | Partial rendering cache | One entry per drawn item, released by `free_graphics_resources`. |
   | Box shadows, item cache | Owned by the item. Cleared at a scale change and by `free_graphics_resources`. |

3. **One bitmap per image and size.** The same icon shows up to 24 times. It is
   decoded and uploaded once.
4. **Idle trim.** A one-second timer calls `ClearResources` after 1.5 s with no frame.
5. **Suspend releases everything.** A hidden window keeps no device.

Before you add a cache, give it a bound or an owner. Then measure idle again
(section 8).

## 6. Selection and fallback

Spec 12 section 3.1 has the rules and their order. In short: `MELLO_RENDERER`
overrides, the "Hardware Acceleration" setting off means `skia-software`, a failed
`probe()` means `skia-software`, else `d2d`. The renderer then uses the device that
`probe()` created.

`skia-software` must stay in the build. It is the fallback for a missing GPU driver,
a remote desktop session and a driver update.

## 7. Debug switches

The switches keep the `FX_D2D_` prefix of the lab, where the renderer was built.

| Variable | Effect |
|---|---|
| `MELLO_RENDERER=d2d` or `skia-software` | Forces the renderer (spec 12 section 3.1). |
| `FX_D2D_STATS=1` | One line per second on stderr: frames, walk, flush (`EndDraw`), present, interval, p50 / p95 / max in ms, and the time per item kind. |
| `FX_D2D_DEBUG=1` | Logs failed Direct2D calls, each idle trim, the surface rebuild time after a lost device, and each frame's buffer age and drawn box. |
| `FX_D2D_FULL_REDRAW=1` | Turns partial rendering off: every frame draws the whole window. For A/B runs. |
| `FX_D2D_FAKE_DEVICE_LOSS=N` | Every N frames, takes the lost-device path as if `Present` returned `DXGI_ERROR_DEVICE_REMOVED`. Windows has no API that removes a D3D11 device on request. |
| `FX_D2D_NO_LAYERS=1` | Draws opacity groups and rounded clips without `PushLayer`. For memory A/B runs. |
| `FX_D2D_NO_SHADOWS=1` | Skips box shadows. For memory A/B runs. |
| `FX_D2D_TEXT_SNAP_Y=0` or `1` | Turns the baseline snap off or on. |
| `FX_D2D_TEXT_PARAMS=1`, with `FX_D2D_TEXT_GAMMA`, `_CONTRAST`, `_GRAY_CONTRAST`, `_MODE`, `_GRIDFIT` | Custom DirectWrite rendering parameters, for text A/B runs. |

## 8. Measured

mello, Windows, release build as `client-dev-release.sh` makes it, idle, window
shown, no voice channel, 2026-10-30 and 2026-10-31:

| Renderer | Working set | Private working set | GPU dedicated + shared |
|---|---|---|---|
| `d2d` (default) | 117 MB | **54 MB** | 33 + 2 MB |
| `skia-software` (fallback), same sitting | 108 MB | 49 MB | 11 + 1 MB |
| Skia on wgpu (D3D12), not shipped | 224 MB | 133 MB | 117 + 65 MB |
| Skia on OpenGL, not shipped | 227 MB | 144 MB | 59 + 16 MB |
| FemtoVG on OpenGL, not shipped | 235 MB | 154 MB | 52 + 26 MB |

The three rows that are not shipped are from the first sitting. In that sitting
`skia-software` measured 57 MB private. The 11 MB of GPU memory on the CPU path is
the device of the HUD overlay.

m3llo-fx-lab feed bench, 144 Hz, CPU time per frame from Slint's tree walk to
`EndDraw`, p50 / p95:

| Phase | `d2d` | `d2d` fps |
|---|---|---|
| Belt fly and flip | 6.9 / 9.4 ms | 123 - 137 |
| Belt burst | 6.4 / 7.1 ms | 140 - 145 |
| Recap reveal | 4.0 / 4.9 ms | 144 |
| Recap at rest | 2.5 / 2.9 ms | 144 |

`skia-software` runs the same bench at 60 - 70 fps with 14 - 17 ms per frame. The
first frame of a new process costs 10 - 15 ms on `d2d`, and 45 - 58 ms on Skia on
wgpu.

Pixels: 61 capture frames of every lab tab differ from `skia-software` on 0.1 - 1.5 %
of the pixels, at glyph and shape edges only. At scale factor 1.5 the difference is
0.9 - 1.1 %.

### 8.1 Idle in a voice channel

The 100 MB rule. `scripts/perf/voice-idle-memory.ts`, release build with the e2e
feature, local stack and SFU, two users in one crew, alice sampled every 2 s for
60 s after 15 s to settle, window shown, 2026-11-01:

| Renderer | Logged in, idle | Idle in a call | GPU memory |
|---|---|---|---|
| `d2d` (with partial rendering) | 73 MB | **83 MB** | 38 MB |
| `d2d` (before partial rendering) | 75 MB | 85 MB | 39 MB |
| `skia-software` | 65 MB | 76 MB | 11 MB |

The e2e feature costs nothing measurable: a plain and an e2e release build both
measured 52 MB on the first screen. Idle in a call, the window draws two frames every
30 s (a clock label). Nothing else redraws.

### 8.2 The sealed reveal at rest

The belt held open in the centre, 80 embers animated, 144 Hz.
`scripts/perf/reveal-frame-cost.ts`, `FX_D2D_STATS=1`, 2026-11-01:

| Build | Frame (walk p50) | Rectangles | Text | fps | CPU (Task Manager) |
|---|---|---|---|---|---|
| Before the brush and path caches | 6.7 ms | 1.8 ms | 0.9 ms | 121 - 138 | |
| Caches, full redraw | 4.5 ms | 1.0 ms | 0.7 ms | 144 | 6.2 % |
| Caches, partial rendering (shipped) | 4.3 ms | 0.7 ms | 0.3 ms | 144 | 5.2 % |
| `skia-software` | | | | | 6.2 % |

CPU is the share of the whole machine (16 threads). 5.2 % is 83 % of one core. The
rest of the frame is Slint's own work: 80 embers with about 15 bound properties each,
evaluated 144 times a second. The renderer cannot remove that work.

In the lab's feed bench, partial rendering changes nothing measurable: the belt and
recap animations cover most of the window.

### 8.3 A lost device

`FX_D2D_FAKE_DEVICE_LOSS` on the lab's feed bench: nine and thirteen losses in 20 s.
Every frame after a loss drew. Private working set stayed between 29 and 34 MB with
no upward trend; the run without losses ended at 34 MB.

## 9. Tests and checks

- `client/src/renderer_choice.rs` has unit tests for every selection rule. They run on
  every platform. Keep them passing.
- The renderer itself has no tests in mello. The checks run in m3llo-fx-lab:
  `win\pixel-compare.py` (capture frames against `skia-software`),
  `win\mem-reveal.ps1` and `win\mem-summary.py` (the memory matrix), and the feed
  bench with `FX_D2D_STATS=1`.
- In mello, on a release build with `--features development,e2e` and
  `SLINT_EMIT_DEBUG_INFO=1` (the client build script then embeds Slint's element
  metadata in a release build), with the local stack and SFU:

  ```bash
  MELLO_BIN=target/release/mello.exe MELLO_RENDERER=d2d \
    node tools/mello-driver/src/cli.ts run scripts/perf/voice-idle-memory.ts
  MELLO_BIN=target/release/mello.exe MELLO_RENDERER=d2d FX_D2D_STATS=1 \
    node tools/mello-driver/src/cli.ts run scripts/perf/reveal-frame-cost.ts
  ```

- After a change to the drawing code, run the pixel comparison, with and without
  `FX_D2D_FULL_REDRAW=1`. After a change to a cache or to the device, run
  `voice-idle-memory.ts` again. After a change to the device or surface, run the lab
  bench with `FX_D2D_FAKE_DEVICE_LOSS=60`.

## 10. Upgrading Slint

The renderer uses Slint internals: `RendererSealed`, `ItemRenderer`, `GlyphRenderer`,
`ItemCache`, `BoxShadowCache` (i-slint-core feature `box-shadow-cache`) and the
private `WinitCompatibleRenderer` of the winit backend. They are published, not
stable. mello pins `i-slint-backend-winit = "=1.18.1"`.

At each Slint upgrade:

1. Copy the new `i-slint-backend-winit` into `third_party/`. Apply the edits marked
   `mello:` in `lib.rs`, the `renderer-d2d` feature in `Cargo.toml`, and
   `renderer/d2d.rs`.
2. Fix the renderer against the changed core traits.
3. Run the pixel comparison and the idle measurement (section 9).

The hook stays until Slint has a public renderer hook. The 1.18.2 backport in spec 12
section 3.1 is a separate item.

## 11. Open

Verified on 2026-11-01:

- Idle in a voice channel: 83 MB (section 8.1).
- A lost device, through `FX_D2D_FAKE_DEVICE_LOSS` (section 8.3).
- The `PopupWindow` paths: the sign-out (account menu) and create-channel (crew menu)
  journeys pass, and the window is clean after the popup closes.

Not verified:

- The stream pop-out. No e2e journey runs a stream.
- A second monitor at another scale factor. The test machine has one monitor. A fixed
  scale factor of 1.5 was checked in the lab (sections 3.1 and 8).
- A real driver reset (a GPU timeout or a driver update).

### Decided

- **No shared D3D11 device** with the stream presenter or the HUD. The presenter's
  device must be on the adapter that decodes (a discrete GPU on a laptop with two
  GPUs), must be safe for two threads, and exists only during a stream. Sharing it
  would move the window onto the discrete GPU during streams, or break the shared
  frames. It saves one device (about 10 MB) only while a stream or the HUD runs.
- **No single DirectComposition tree.** The presenter's `topmost` target already
  composes the video over the swapchain.

### Later

| Item | What | Why |
|---|---|---|
| Upstream | Ask Slint for a public renderer hook, or a `renderer-d2d` in Slint. | Then the fork of the winit backend can go. |
| UI | Fewer embers, or a lower update rate for them, in the sealed reveal. | The remaining 5 % CPU at rest is Slint's property work, not drawing. |
