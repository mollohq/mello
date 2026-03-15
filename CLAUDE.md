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

## Before Calling Something Done
- Run tests: `cargo test --workspace` (Rust), `cd libmello && cmake --build build && ctest` (C++)
- Fix all warnings — treat warnings as errors in this codebase.
- Check for regressions in adjacent code you touched.
- If you added behavior, add a test for it. Tests live next to the code they cover.

## Git & PRs
- Commit often with clear messages: `feat(voice): add VAD threshold config`
- Use conventional commits: feat / fix / refactor / test / docs / chore
- Never push directly to main. Always work on a branch.
- When you think a PR is ready, say so and list what it does — then wait for the Human Operator to say "open it."
- Do not open PRs autonomously.

## Rust Standards (mello-core, client)
- `clippy` must pass: `cargo clippy --all-targets -- -D warnings`
- No `unwrap()` in non-test code — use `?`, `expect("reason")`, or proper error handling.
- Keep `async` minimal — prefer structured concurrency over spawning loose tasks.
- Log at every state transition using `log::info!/debug!/warn!/error!` — see Architecture §15.
- Public API must have doc comments. Internal functions: comment the *why*, not the *what*.

## C++ Standards (libmello)
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
- Prefer P2P for media; only touch the server for signaling and state.

## Slint UI Rules
- `Image` has NO `vertical-alignment` property — only `Text` does.
  To vertically center an `Image` inside a layout or container, use:
  `y: (parent.height - self.height) / 2;`
  This is the established pattern throughout the codebase (see control_bar.slint, voice_channel_view.slint, settings_modal.slint).

## What Not To Do
- Do not change existing public API signatures without flagging it first.
- Do not add new Cargo/CMake/npm dependencies without asking.
- Do not touch the FFI boundary (`mello-core-sys/`) without reading spec 03-LIBMELLO.md.
- Do not introduce async runtimes in libmello — it is synchronous C++ by design.
- Do not refactor working code as part of a feature PR. Separate commits, separate PR.
```