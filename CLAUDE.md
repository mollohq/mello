```markdown
# CLAUDE.md — Mello / Mollo Tech AB

## What This Is
Mello: Discord-meets-Parsec. Rust client (Slint UI + mello-core), C++ low-level lib (libmello),
Go/Nakama backend. Open Core. Windows-first beta. See `specs/` for every design decision.

## The Golden Rule: Read the Specs First
Before implementing anything non-trivial, locate and read the relevant spec in `specs/XX-NAME.md`.
If the spec contradicts your plan, ask what to do. If the spec is silent, ask before inventing.
Never re-implement something that already has a spec — check first.

## Learning From the Human Operator
When the Human Operator corrects you or says "don't do that" / "we do it this way":
1. Apply the fix immediately.
2. If it's a pattern, say: "Should I add this to CLAUDE.md?" and wait for a yes/no.
Do not repeat a corrected mistake in the same session.

## Writing Standards: Simplified Technical English

Use **ASD-STE100 Simplified Technical English (STE)** in all specs, and in all
agent communication with the Human Operator.

- One idea per sentence. Maximum 20 words for an instruction, 25 for a description.
- Active voice. Present tense. Keep the articles (`the`, `a`).
- One word, one meaning. Do not use synonyms for variety.
- No metaphors, no idioms, no figurative language.
- No editorializing: drop `honestly`, `genuinely`, `deliberately`, `worth noting`,
  `the one thing that`, `it is worth stating`. State the fact and stop.
- Do not narrate the writing. Do not praise or dramatize the content.
- Keep paragraphs short. Prefer a table or a list over prose.

Write what the reader must know. Delete the rest.

## Before Calling Something Done
Run the gate. Do not hand-roll the individual commands — the script is what CI
and the pre-push hook run, so anything else can disagree with them.

```bash
./scripts/check.sh        # ~60s: fmt, clippy, all Rust tests, RPC contract
./scripts/check-full.sh   # adds libmello ctest and the Nakama Go modules
```

- `check.sh` also runs as the pre-push hook (`git config core.hooksPath .githooks`).
- Run `check-full.sh` when you touch `libmello/` or `backend/`, and before a release.
- **`CI=true` is mandatory** and the scripts set it. Without it, hardware-dependent
  voice/video tests block forever waiting on real devices, or die with SIGTRAP on a
  machine without screen-recording permission. Per-crate runs that avoid that
  hardware (e.g. `cargo test -p mello-core --lib`) are fine without it.
- **Clippy must be `--workspace`.** Plain `cargo clippy` only checks
  `default-members` and silently skips every crate under `tools/`. A lint error
  hid there until it blocked a release.
- Fix all warnings — treat warnings as errors in this codebase.
- Check for regressions in adjacent code you touched.
- If you added behavior, add a test for it. Tests live next to the code they cover.

## Testing
Read `TESTING.md` before adding tests. In short:

- **UI behavior** — use the headless harness in `client/src/testkit.rs`. It drives
  the real `callbacks::wire_all` and `handlers::handle_event`, so tests fail when
  production wiring changes. Inject an `Event` and assert on `MainWindow`; invoke a
  callback and assert on the emitted `Command`s. Journey tests live in
  `client/src/flow_tests.rs`.
- **Assert structurally, not via accessibility.** These panels declare almost no
  `accessible-role`, so `accessible_enabled()` reads as absent even on a healthy
  screen. Query component type names instead.
- **Prove a new regression test actually fails** without its fix. A test that
  cannot fail is decoration. `./scripts/mutation-check.sh` checks the suite still
  catches deliberate breakage.
- **Flaky means broken.** Fix the determinism or delete the test. Never add
  retries. Every flaky test found here waited on one thing and asserted on
  another that lagged it.
- **Never suppress a gate to make it green** — no `continue-on-error`, no
  disabling a lane. If a check must be skipped, exclude the one named test and
  leave the rest gating.

## Git & PRs
- Commit often with clear messages: `feat(voice): add VAD threshold config`
- Use conventional commits: feat / fix / refactor / test / docs / chore
- **Never add `Co-Authored-By` trailers or any AI co-authorship attribution to commits.** These are the Human Operator's commits under their name.
- Never push directly to main. Always work on a branch.
- When you think a PR is ready, say so and list what it does — then wait for the Human Operator to say "open it."
- Do not open PRs autonomously.

## Rust Standards (mello-core, client)
- `cargo fmt --all` must pass before committing. Check with `cargo fmt --all -- --check`.
- `clippy` must pass before comitting: `cargo clippy --all-targets -- -D warnings`
- No `unwrap()` in non-test code — use `?`, `expect("reason")`, or proper error handling.
- Keep `async` minimal — prefer structured concurrency over spawning loose tasks.
- Log at every state transition using `log::info!/debug!/warn!/error!` — see Architecture §15.
- Public API must have doc comments. Internal functions: comment the *why*, not the *what*.

## C++ Standards (libmello)
- **Before touching any file in `libmello/src/audio/` or `libmello/src/video/`, read `specs/03-LIBMELLO.md` in full.** These pipelines have threading, COM, and callback invariants that are not obvious from the code alone. Violating them causes silent failures on Windows (no crash, no error, just no audio/video).
- C++17. No raw owning pointers — use `std::unique_ptr` / `std::shared_ptr`.
- Thread safety: document which threads call each function. Use `MELLO_LOG_*` macros freely.
- RAII everywhere. No manual `new`/`delete`.
- Keep the C ABI surface in `mello.h` minimal and stable — changes break the Rust FFI layer.

## Go / Nakama Standards (backend)
- Nakama modules live in `backend/modules/`. Write Go, not Lua/TS.
- Keep modules stateless where possible — state lives in Nakama storage or PostgreSQL.
- Every RPC and hook must validate its input and return typed errors.
- Test with the local Docker stack (`docker-compose up`) before assuming it works on Render.

## Scale & Performance Mindset
- Target metrics are hard constraints, not aspirations (see Architecture §2 and §13).
  - Client: <100MB install, <100MB RAM, <3s cold start.
  - P2P: <50ms voice latency, >90% NAT traversal success.
- Before adding a dependency: will it fit inside the size/RAM budget? Check binary size impact.

## The Design System
`designs/design-system.html` is the source of truth for every visual decision.
Open it before changing any UI, and follow it. If a change needs something the
system does not have, ask — do not invent a colour, a shape or a type size.

The rules it carries, in short:

- **Red `#FF453A` is state**: live, recording, unread, the 3 in the mark. It is
  also the fill of a button that commits — create, join, save, sign in, update,
  end a stream.
- **White is action**: everything else you press, the active channel, focus.
- **Green `#3FD07A` is one thing**: the person speaking, on their name.
- **Identity colours** are for people, crews and games. Never on chrome.
- **Shape**: a card cuts one corner, a button cuts top-left and bottom-right, a
  person is an octagon, a crew or game is a two-corner tile, a field is a plain
  rectangle. Radii survive only on dots, discs and pips.
- **Type**: Oxanium names it or it is big; Barlow is everything else, in a 300
  sentence register and a 600 label register with tracking.
- **Content keeps its colour.** Game footage is the only saturated thing on
  screen. Chrome laid over it gets black washes, not the graphite ramp.

Two failure modes have each cost a day. Check for both:

- **A white fill with a white label.** `Theme.accent` resolves to white now, so
  any control still pairing it with `#FFFFFF` is invisible. Seven shipped that
  way before a grep found them.
- **A header strip that paints over its panel's cut corner.** A strip in a
  different colour from the panel must carry the panel's shape, inset by the
  keyline, or it erases the outline.

## Slint UI Rules
- Use `MelloTextInput` from `theme.slint` instead of raw `TextInput` — it applies `Theme.selection-bg` / `selection-fg` (accent-tinted highlight) instead of Slint’s default cupertino blue.
- For bordered form fields (settings, modals), use `MelloInputField` — it fills the field height so mouse drag-selection works; do not vertically center a bare `MelloTextInput` with `preferred-height` only.
- Do not put `TextInput` / `MelloTextInput` inside `Flickable` (breaks double/triple-click word selection; slint#6514). Use `MelloScrollArea` for scrollable settings/content instead.
- `Image` has NO `vertical-alignment` property — only `Text` does.
  To vertically center an `Image` inside a layout or container, use:
  `y: (parent.height - self.height) / 2;`
  This is the established pattern throughout the codebase (see control_bar.slint, voice_channel_view.slint, settings_modal.slint).
- **HorizontalLayout forces `y: 0` on direct children.** Setting `y:` on a direct child of
  `HorizontalLayout` is silently overridden — the element sticks to the top.
  **Preferred (Slint 1.17+):** set `cross-axis-alignment: center;` on the `HorizontalLayout`
  (or `VerticalLayout`) — it centers each child on the cross axis at its preferred size. Note this
  applies to *all* children of that layout, so only use it when every child should be centered.
  For the legacy per-element approach (or when siblings need different alignment), wrap it:
  ```
  // WRONG — y is ignored, button sits at top
  HorizontalLayout {
      Rectangle { width: 40px; height: 40px; y: (parent.height - self.height) / 2; }
  }
  // RIGHT — outer rect stretches, inner rect centers inside it
  HorizontalLayout {
      Rectangle {
          width: 40px;
          Rectangle { width: 40px; height: 40px; y: (parent.height - self.height) / 2; }
      }
  }
  ```
- **Progress/fill bars must set `x: 0;`** on the fill `Rectangle`. A child `Rectangle` with
  `width` smaller than its parent defaults to **horizontally centered** in Slint, so omitting
  `x: 0` makes the bar grow from the center outward. See `debug_panel.slint` `StatBar` for
  the canonical pattern (`x: 0`, `clip: true` on the track).
- **The bundled fonts carry no symbol glyphs.** Oxanium and Barlow have letters,
  digits, the middot and dashes. They do not have `✂ ▾ ✓ ▶ › ●`. A symbol
  written as text renders as a tofu box. Use an SVG from `client/ui/icons/`
  with `colorize:`, or a `Rectangle` for a dot.
- **`x` and `y` belong to the layout.** Setting `x` on a direct child of a
  `HorizontalLayout` is a compile error; setting `y` is silently ignored. Use
  `cross-axis-alignment: center` on the layout, or wrap the child.
- When a design mockup (`designs/*.html`) contains inline SVGs, **do not** try to recreate them
  with Slint rectangles or shapes. Instead, extract the SVG into `client/ui/icons/<name>.svg`
  (stroke="black", no hardcoded colors) and reference it with `@image-url("../icons/<name>.svg")`
  + `colorize:` for theming. Slint renders SVG icons cleanly; hand-drawn rectangle approximations
  look broken.
- To push an element to the far right in a `HorizontalLayout`, do NOT use `alignment: start` —
  it overrides stretch. Instead, remove `alignment`, set `horizontal-stretch: 1` on the element
  that should fill the middle, and `horizontal-stretch: 0` on fixed items. See control_bar.slint
  for the canonical pattern. Same applies vertically with `vertical-stretch`.

## Verifying Slint UI Work
Never call a UI change done without looking at a render. Reading the code does
not show a clipped label or a square corner.

**Compile one file (~1s).** A full `cargo check -p mello-client` takes a minute.
```bash
slint-viewer --check client/ui/panels/crew_panel.slint
```

**Render one file headless (no app, no display).** Every panel exports a
`Preview` component with mock data, so this shows the real layout.
```bash
slint-viewer --screenshot out.png client/ui/panels/crew_panel.slint
```
Needs `slint-viewer` 1.17+. Check `slint-viewer --version` matches the Slint
version in `client/Cargo.toml`; older builds have neither flag.

**Drive the running app with real data.** The client compiles an embedded MCP
server behind its own `mcp` feature (`client/src/lib.rs`, `mcp_server::init`).
It is a no-op unless `SLINT_MCP_PORT` is set.
```bash
SLINT_EMIT_DEBUG_INFO=1 SLINT_MCP_PORT=9315 cargo run -p mello-client --no-default-features --features production,mcp
```
Then speak JSON-RPC to `127.0.0.1:9315/mcp`. The `Accept` header is required.
```bash
curl -s -X POST http://127.0.0.1:9315/mcp \
  -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```
Tools include `take_screenshot`, `get_element_tree`, `find_elements_by_id`,
`click_element`, `dispatch_key_event` and `set_element_value`.
`take_screenshot` returns base64: write the response to a file and decode it,
because the payload breaks inline JSON parsing.

Enabling the feature rebuilds the `slint` crate. Run one cargo process at a
time, or the second blocks on the artifact lock.

## Running the Client
- Always use `.\client-prod.ps1` to start the client — there is no local backend.

## Troubleshooting
- Be systematic. Never throw changes at the wall to see what sticks.
- Start from the simplest working state (e.g. a plain window, a basic request).
- Add one thing at a time, verifying each step before adding the next.
- Form a hypothesis, test it, confirm or reject, then move on. Don't guess.
- If something doesn't work, isolate the variable — don't change multiple things at once.

## Fix It Right
- Never apply band-aid / "simple" fixes. Always implement the proper, robust solution.
- If a quick hack is tempting, stop and think about the real root cause first.
- If unsure what the proper fix is, ask - don't ship a workaround.

## What Not To Do
- Do not change existing public API signatures without flagging it first.
- Do not add new Cargo/CMake/npm dependencies without asking.
- Do not touch the FFI boundary (`mello-core-sys/`) without reading spec 03-LIBMELLO.md.
- Do not introduce async runtimes in libmello — it is synchronous C++ by design.
- Do not refactor working code as part of a feature PR. Separate commits, separate PR.
```