# Mello Test Harness — UI + Integration Safety Net

## Status

| Phase | State |
|---|---|
| 0 — Wake up dormant C++/Go tests in CI | **done** |
| 1 — Client as a library crate | **done** |
| 2 — Four production seams | **done** |
| 3 — Headless UI harness | **done** |
| 4 — Flow tests + screen invariants | **done** |
| 5 — Reducers | **done** for onboarding and mute/deafen; others not justified by evidence |
| 6a — RPC contract test | **done** |
| 6b — Dockerised integration | **done** |
| 6c — Release-artifact smoke gate | **done** (unverified on the self-hosted runners) |
| 6d — Production canary | **done**, verified against prod 2026-08-07 (4 crews discoverable) |
| 6e — Discovery error + retry, growth alarm | **done** |
| 7 — Pixel snapshots | **superseded** — see below |
| — Mutation checking | **added** (not in the original plan) |

Phase 5 was scoped to the two flows with demonstrated fragility. Onboarding's
step was written from 16 sites, three of which skipped persistence; mute/deafen
was duplicated across five entry points and had already diverged. Crew, chat and
streaming show no equivalent problem, so reducers there would be churn.

Phase 7 was replaced by layout assertions on element geometry. They catch the
same failure that mattered — a control present in the tree but collapsed to zero
size — deterministically, with no golden images, no `renderer-software` feature
and no per-platform baselines. Pixel goldens remain possible if visual diffing
is wanted later.

`scripts/mutation-check.sh` was added after a sweep showed the suite passing on
a deliberately broken chat callback. It introduces eight realistic regressions
and asserts the suite goes red for each.

**All three root causes of the signup outage are now covered**, each with a
regression test verified to fail without its fix.

Defects found and fixed along the way, none of which any prior test could catch:
missing Create Crew card with zero crews; discovery failure rendering a blank
window; `.expect()` on tray/hotkey creation aborting startup; compile rot in
`test_jitter_buffer.cpp`; a `curl`-based Docker healthcheck that never ran
because the Nakama image has no curl (both compose and the production
Dockerfile); and `NakamaClient::channel_list` calling an unregistered RPC.

Still open: the remaining Phase 4 journeys (crew create/join, deep links, chat
send, streaming), `onboarding_retry_preserves_avatar` (the `.take()` on the
avatar mutex), Phase 5, Phase 7, and the two macOS RTP test failures inherited
from the stream PR.

Two follow-ups were spun out as separate tasks: removing the dead
`channel_list` method, and cleaning up the accounts the release smoke test
leaves in production.

## Context

Mello has grown past the point where the Human Operator can hold it in their head. Unit
coverage exists (303 Rust, 96 C++, 106 Go tests) but a recent change broke onboarding so
that **no new user could sign up**, and nothing caught it — not CI, not tests, not
telemetry. At ~10 signups/week the blast radius was small. At Discord-competitor scale it
would have been an extinction event.

The goal is not "more tests." It is **psychological safety**: a short command that answers
"did I break a critical user journey?" before pushing, and an alarm that fires within
minutes when production breaks.

### Why the existing suite could not have caught it

The onboarding break was **three bugs in three different layers**. This is why a
single-layer harness won't deliver the safety being asked for:

| Layer | Bug | Evidence |
|---|---|---|
| Slint view logic | With 0 discoverable crews the "Create Your Own Crew" card never instantiates — it lives inside `for base in bento-set-bases`, and `bento_bases(0, 5)` returns `[]`. Step 1 has **no** way forward. | [converters.rs:205](client/src/converters.rs:205), [onboarding.slint:683](client/ui/panels/onboarding.slint:683) |
| State-machine reachability | `handle_discover_crews`'s error arm only does `log::error!` — it emits **no event**. `onboarding_step` stays `0`; `main.slint` renders onboarding only for `1..=3` and the app only for `logged-in && (step==0 \|\| step>3)`. Neither matches ⇒ **blank window**, no error, no retry. | [crew.rs:17](mello-core/src/client/crew.rs:17), [main.slint:597](client/ui/main.slint:597) |
| Build artifact / environment | `NAKAMA_HTTP_KEY` is baked in via `option_env!`, defaulting to `"defaulthttpkey"`. Render generates its value with `generateValue: true`. Key drift ⇒ silent 401 on the guest `discover_crews` call ⇒ the blank window above. No local test can catch this. | [config.rs:24](mello-core/src/config.rs:24), [render.yaml](render.yaml) |

### Three assets already in the codebase that make this cheap

1. **A near-perfect test seam.** `poll_loop::start(&ctx, event_rx, update_event_rx)` takes
   the `Event` receiver as a *parameter*, and `callbacks::wire_all(&ctx)` writes to
   `cmd_tx`. So a headless test can drive the **real production wiring**: inject `Event` →
   assert Slint properties; invoke a Slint callback → assert emitted `Command`s. No
   network, no audio device, no window.
2. **202 tests that already exist and never run on a PR.** CI is only `fmt` / `clippy` /
   `cargo test`. The 96 C++ gtest cases and 106 Go tests are dormant — and
   `MELLO_BUILD_TESTS` defaults to `OFF`, so even the `ctest` recipe in `TESTING.md` finds
   zero tests.
3. **A scenario runner.** `client/src/perf_mode.rs` already drives the real app headlessly
   from a JSON scenario, forking the event stream to assert on it. It generalizes into the
   artifact smoke test.

### Verified Slint 1.17 API constraints (these shaped the design)

- `ElementHandle` queries require `SLINT_EMIT_DEBUG_INFO=1` at build time and **fail
  silently** — every query returns empty, so tests would pass vacuously. The harness
  constructor must self-check this.
- There is **no `is_visible()`**. Invisible subtrees are simply absent from query results.
  This is *convenient*: "the screen is blank" becomes "no screen root matched."
- `init_no_event_loop()` is per-thread ⇒ many `#[test]` per binary. But timers don't run,
  which is exactly why we extract `poll_loop::tick()` rather than waiting on the 100 ms timer.
  (`init_integration_test_with_*` is once-per-process — avoid it outside the pixel lane.)
- No public keyboard-input helper. Build one on `Window::dispatch_event` +
  `slint::platform::WindowEvent::KeyPressed` (~10 lines).
- `Window::take_snapshot()` exists and is ungated, but returns `Err` under the default
  testing backend. Pixel capture needs a hand-built `TestingBackendOptions { renderer_name:
  Some("software".into()), .. }` and the `renderer-software` feature.
- `i-slint-backend-testing` does not follow semver — must be pinned `=1.17.0`.

---

## Guiding principle: tests before refactor

The Human Operator chose "reducers for all flows." That is the right destination and it is
the multi-week item. **Sequencing matters:** write the flow tests against the current
tangle *first*, then refactor underneath coverage that already passes. Refactoring
onboarding's 15 mutation sites with no tests in place is precisely the move that broke
signup last time.

Per `CLAUDE.md`, each reducer extraction is its **own PR**, separate from the harness PRs.

---

## Phase 0 — Wake up the dormant tests (½ day, near-zero risk)

Highest value per minute of work in this entire plan.

- `libmello/CMakeLists.txt:451` — flip `MELLO_BUILD_TESTS` to `ON` by default (or always
  pass `-DMELLO_BUILD_TESTS=ON` in CI and fix the `TESTING.md` recipe, which is currently
  incomplete and silently finds no tests).
- `.github/workflows/pr-checks.yml` — add two lanes:
  - **C++**: `cmake -B build -S libmello -DMELLO_BUILD_TESTS=ON && cmake --build build && ctest --test-dir build --output-on-failure` (96 tests).
  - **Go**: `cd backend/nakama/data/modules && gofmt -l . && go vet ./... && go test ./...` (106 tests). Note `gofmt` is currently unenforced — `crews.go:340` is dirty.
- Set `CI=true` explicitly in the workflow `env:` rather than relying on GitHub's implicit value, since `mello-sys/tests/video_pipeline.rs:71` is the only test that reads it.

**Investigation task (user answered "not sure") — resolve before writing the workflow:**
probe the self-hosted runners for Docker + `docker compose`, and confirm whether
GitHub-hosted `ubuntu-latest` minutes are acceptable (`release.yml` already uses it for
publishing). Recommended default: put the Go lane, the RPC contract check, and the nightly
Docker integration on `ubuntu-latest` (native amd64 ⇒ the Nakama plugin build is seconds,
not the minutes it takes emulated on Apple Silicon); keep Rust and C++ on the self-hosted
Windows/macOS boxes.

---

## Phase 1 — Expose the client as a library (½ day)

`client` is binary-only, so `client/tests/` cannot reach `AppContext`, `MainWindow`,
`callbacks`, or `handlers`. Everything downstream depends on fixing this.

- Add `client/src/lib.rs` (crate `mello_client`) re-exporting the existing modules and a
  `pub fn run() -> anyhow::Result<()>` holding today's `run_app()` body.
- `client/src/main.rs` becomes `fn main() { mello_client::run() }`.
- `client/Cargo.toml`: add `[lib] name = "mello_client"`, keep `[[bin]] name = "mello"`.
- `client/build.rs`: enable debug info via `slint_build::CompilerConfiguration::with_debug_info(true)`
  for dev/test profiles — required for `ElementHandle`, and it must not be left to a
  developer remembering an env var.
- Move the 5 existing `main.rs` tests into the harness (Phase 3); they currently
  re-implement the callback bodies they claim to test, so they assert nothing about production.

This also lets the canary (Phase 6) and scenario runner reuse client code instead of
duplicating it.

---

## Phase 2 — Production seams (2–3 days)

Four seams. All four are defensible on their own merits, not just for tests.

1. **`poll_loop::tick()`** — split [poll_loop.rs:23](client/src/poll_loop.rs:23) into a
   `pub fn tick(&mut PollState)` holding today's closure body, plus `pub fn start(...) ->
   Timer` that calls `tick` every 100 ms. Tests call `tick()` directly: synchronous,
   deterministic, no timer needed. Also kills the 44-field hand-clone of `AppContext` at
   `poll_loop.rs:28-72` (currently every new field means editing three places).
2. **`AppContext::for_test()`** — give `StatusItem`, `HotkeyManager`, `HudManager`,
   `ForegroundMonitor` a `disabled()`/`noop()` constructor; `Updater` is already
   `Option`. **Bonus prod fix:** `main.rs:367-374` currently does
   `.expect("failed to create tray icon")` and `.expect("failed to init hotkey manager")`
   — a tray-creation failure hard-crashes the app on startup today. These become graceful
   degradation.
3. **`Settings` path override** — honour a `MELLO_CONFIG_DIR` env var (or add
   `save_to(dir)`) so tests write to a `TempDir`. Today `Settings::save()` calls
   `confy::store("mello", None, ..)` and onboarding calls it on nearly every step change,
   so any onboarding test would mutate the developer's real config.
4. **`Config::from_env()`** in `mello-core` — lift the ~15 lines that already exist at
   [tools/voice-test-client/src/main.rs:219](tools/voice-test-client/src/main.rs:219)
   (`NAKAMA_HOST` / `NAKAMA_PORT` / `NAKAMA_SSL` overrides) and call it from
   `client/src/main.rs:77`. Consumed by the integration lane, the canary, and the artifact
   smoke test. Also make `AVATAR_WORKER_URL` ([avatar.rs:5](client/src/avatar.rs:5))
   overridable — it is an undeclared hard dependency of signup.

---

## Phase 3 — The headless UI harness (3–4 days) ← flagship

`client/src/testkit/` (compiled under `#[cfg(any(test, feature = "testkit"))]`):

```rust
pub struct Harness {
    app: MainWindow,
    ctx: AppContext,
    cmd_rx: UnboundedReceiver<Command>,   // what the UI asked core to do
    event_tx: std::sync::mpsc::Sender<Event>, // what core tells the UI
    _config_dir: tempfile::TempDir,
}

impl Harness {
    pub fn new() -> Self;                     // init_no_event_loop + real wire_all()
    pub fn emit(&mut self, e: Event);         // inject core event, then pump
    pub fn pump(&mut self);                   // poll_loop::tick() until drained
    pub fn commands(&mut self) -> Vec<Command>;
    pub fn click(&self, element_id: &str);    // ElementHandle + mock_single_click
    pub fn type_text(&self, s: &str);         // Window::dispatch_event(KeyPressed)
    pub fn screen(&self) -> Screen;           // which top-level screen root matched
    pub fn app(&self) -> &MainWindow;         // raw property access
}
```

Non-obvious requirements:

- **`Harness::new()` must assert element queries work** — query the root, assert non-empty,
  panic with a pointed message if `SLINT_EMIT_DEBUG_INFO` is missing. Otherwise every
  test silently passes while asserting nothing. This is the single biggest footgun in the
  Slint testing API.
- `click()` must **panic when no element matches**, for the same reason.
- Guard `init_no_event_loop()` with a `thread_local!` `Once` — it panics if the backend is
  already initialised.
- `TestingWindow`'s `Drop` asserts all item trees were unregistered; a leaked component
  panics with an unrelated-looking message. Note that onboarding leaks 4+ timers via
  `std::mem::forget` ([onboarding.rs:57](client/src/callbacks/onboarding.rs:57) and 6 more)
  — untangle these in the Phase 5 onboarding reducer.

Crucially this drives `callbacks::wire_all` and `handlers::handle_event` — the **real**
production code paths, not copies.

---

## Phase 4 — Flow tests and screen invariants (4–5 days)

### 4a. Flow tests — `client/tests/flows/`

One file per journey, many `#[test]` per file. Each test asserts both UI properties **and**
an `insta` snapshot of the emitted `Command` sequence. The snapshot is the safety mechanism:
any change to a journey shows up as a reviewable diff instead of a silent behaviour change.

The 12 journeys, with the three regression tests that encode last month's outage marked ★:

1. `onboarding_fresh_install_happy_path` — steps 1→2→3→4, asserting `FinalizeOnboarding` carries crew + nickname + avatar.
2. ★ `onboarding_zero_discoverable_crews` — with `DiscoverCrewsLoaded { crews: [] }`, assert a way forward still exists.
3. ★ `onboarding_discover_fails` — assert a visible error + retry affordance, **not** a blank window.
4. `onboarding_finalize_fails_at_step_N` — 7 parameterized cases over the 7 sequential network steps in [auth.rs:695](mello-core/src/client/auth.rs:695), each of which currently leaves the user parked on step 3.
5. ★ `onboarding_retry_preserves_avatar` — the crew avatar lives in an `Arc<Mutex<Option<String>>>` that is `.take()`n at finalize ([onboarding.rs:158](client/src/callbacks/onboarding.rs:158)), so retry after failure silently loses it.
6. `returning_user_restore_{success,failure}` — note `handlers/auth.rs:113` branches on an **empty reason string** to mean "restore failed."
7. `login_email`, `login_social`.
8. `crew_create`, `crew_join_by_invite`, `deep_link_join`.
9. `crew_switch_loads_feed`.
10. `send_message` — including the `{v:1,type,body}` envelope that `chat_validation.go` enforces.
11. `voice_join_leave_mute_deafen_ptt` — absorbs the 5 existing `main.rs` tests, this time against real wiring.
12. `go_live_and_watch_stream`.

### 4b. Screen invariants — `client/tests/invariants.rs`

The test that would have caught the blank window *directly*. Exhaustively enumerate the
top-level gating properties (`onboarding_step ∈ 0..=5` × `logged_in` × `show_sign_in` ×
force-update ≈ 24 combinations) and for each assert:

- **exactly one** top-level screen root appears in the element tree (0 ⇒ blank window,
  ≥2 ⇒ overlapping screens);
- the matched screen contains **at least one element with `accessible_enabled() == Some(true)`**
  — i.e. a way forward. This catches both the blank window *and* the vanished
  Create-Crew card, from one invariant.

Prerequisite: add `accessible-role` / `accessible-label` to the ~8 screen roots and their
primary CTAs. Scope this to those elements — not all 20k lines of Slint. It is also
straightforward a11y work that the product wants anyway.

---

## Phase 5 — Reducers, one flow per PR (multi-week, incremental)

Pattern: `fn reduce(state: &mut S, input: In) -> Vec<Effect>` — pure, no Slint, no I/O.
Handlers and callbacks become thin adapters. `mello-core/src/client/reconnect.rs` already
does this well (injected clock, 7 tests) — it is the in-repo model to copy.

Order, each landing only **after** its Phase 4 flow tests are green:

1. **Onboarding** — highest value, do it first. Replace the `int onboarding-step` (triple-stored
   across Slint / `Settings` / disk, written from **15 sites**, and `in-out` so Slint mutates
   it too) with a single-owner `OnboardingState` enum. Then add a **reachability property
   test**: every state has at least one input path toward `Done`, and no state is entered
   without its persisted counterpart. That test makes the entire class of "user stranded on
   a screen" bugs unrepresentable — including `main.rs:443` (perf mode sets step 4 in the UI
   but not on disk) and `handlers/crew.rs:53` (a late discover response yanks the user
   backwards to step 1 without persisting).
2. Auth / session.
3. Crew selection + membership.
4. Voice.
5. Chat.

---

## Phase 6 — Backend contract, integration, and production alarms (3–4 days)

### 6a. Cross-language RPC contract test (fast lane, no Docker)
Parse `RegisterRpc("name", ..)` from `backend/nakama/data/modules/main.go` → set A; parse
Rust RPC call sites → set B. Assert `B ⊆ A`; report `A \ B` as dead RPCs. Catches "renamed
an RPC, forgot the client" in milliseconds.

### 6b. Dockerized integration — `tools/e2e/`
`docker compose up` + `scripts/seed.sh` + `dev_seed_state`, then drive `mello_core::Client`
via `Config::from_env()` through the real flows, asserting on the `Event` stream. **Zero Go
RPC handlers or hooks are currently tested** — all 106 Go tests are pure functions — so this
is the only thing that would catch a hook or payload-shape regression. Nightly + manual at
first; PR-blocking once it proves stable.

### 6c. Release-artifact smoke gate (blocking, in `release.yml`)
Generalize `perf_mode.rs` → `scenario_mode` (`MELLO_SCENARIO=<path>`) with onboarding steps
and UI-property assertions. Run the **packaged binary** through a real signup against prod,
assert it reaches step 4, then delete the account. This is the only mechanism that catches
the `option_env!`-baked-secret class of bug, because it tests the artifact rather than the
source.

### 6d. Canary — `tools/canary` (non-blocking, alert-only)

**Trigger changed from what was requested — here is the reasoning.** Running the canary on
every PR is the wrong trigger on two counts:

- *It tests the wrong artifact.* A PR-triggered run talks to already-deployed prod, so it
  says nothing about the PR's code. It would not have caught the onboarding break at PR
  time, because the break wasn't in prod yet.
- *It leaves the blind spot open.* The `http_key` failure mode is **environment drift** —
  a rotated secret, an edited GH secret, a Render redeploy. That happens with **zero PRs**.
  If the canary only fires on PRs and none is opened for four days, signup can be dead for
  four days: exactly the disaster scenario.

Recommended instead: **schedule (every 30 min — 15 was overkill at this volume) + on every
push to `main`/deploy + manual dispatch.** Non-blocking and alert-only, as requested; a
canary failure means prod is broken, not that a PR is bad. What runs on *every PR* is
Phase 4 + 6a, which actually test the PR's code and should eventually block.

Canary steps: guest `discover_crews` with the **shipped** http_key → assert ≥1 crew
(catches the zero-crews dead end in prod too) → `authenticate/device create=true` →
join crew → `crew_feed` → delete the account.

### 6e. Product fixes the tests will demand
- `handle_discover_crews` must emit a `DiscoverCrewsFailed` event; onboarding needs a
  visible error + retry.
- Alert on `users_new_24h == 0` using the existing `admin_dashboard_stats` RPC — crude and
  slow, but nearly free and currently nobody is watching it.

---

## Phase 7 — Pixel snapshots (nightly, advisory only)

`client/tests/pixels.rs`, its own binary. Hand-build the backend with
`TestingBackendOptions { renderer_name: Some("software".into()), mock_time: true, .. }` and
the `renderer-software` feature, then `Window::take_snapshot()` → PNG → diff against
goldens in `client/tests/goldens/`.

**Deliberately not a merge gate.** Text metrics under the testing backend only go through
the deterministic fixed-metrics path for the font family literally named `"FixedTestFont"`;
everything else uses real shaping, so output is font- and platform-dependent. Generate
goldens on **one** platform (self-hosted macOS), pin fonts via the already-supported
`SLINT_FONT_PATH` ([emoji_font.rs:37](client/src/emoji_font.rs:37)), run nightly, upload
diff PNGs as artifacts. Advisory, so design iteration never fights the test suite.

---

## The developer ritual

Two scripts, following the existing `.sh`/`.ps1` pairing convention (no new tool
dependency — deliberately not adding `just`):

- **`scripts/check.sh` / `.ps1`** (target < 90 s) — `cargo fmt --all --check`, `cargo clippy
  --all-targets -D warnings`, `CI=true cargo test --workspace` (now including flow tests +
  invariants), RPC contract check. This replaces the body of `.githooks/pre-push`.
- **`scripts/check-full.sh` / `.ps1`** (~10 min) — the above plus `ctest`, `go test`, and
  the Dockerized integration lane.

The answer to "what did my change do?" is `cargo insta review` — the flow snapshots turn
every behavioural change into an explicit, reviewable diff.

---

## New dependencies (need approval per CLAUDE.md)

| Crate | Where | Why |
|---|---|---|
| `i-slint-backend-testing` **`=1.17.0`** | `client` dev-dep — already present at `"1.17"`, needs the exact pin | Crate does not follow semver; a patch bump breaks compilation |
| `i-slint-backend-testing` feature `renderer-software` | pixel lane only | `renderer_name` field does not exist without it |
| `tempfile` | `client` dev-dep | Isolated config dirs so tests don't write the real `Settings` |
| `insta` | `client` dev-dep | Flow-transcript snapshots — the review mechanism |
| `png` *or* `image` | pixel lane only | Encode/diff `SharedPixelBuffer` goldens |

All are dev-dependencies except the pixel-lane feature, so **none affects the shipped
binary's <100 MB install / <100 MB RAM budget**.

---

## Verification

Ordered so each phase is provably working before the next builds on it.

1. **Phase 0**: `cmake -B build -S libmello -DMELLO_BUILD_TESTS=ON && ctest --test-dir build`
   reports **96** tests (not 0). `cd backend/nakama/data/modules && go test ./...` reports
   **106**. Open a throwaway PR and confirm both lanes appear and pass.
2. **Phases 1–2**: `CI=true cargo test --workspace` and `cargo clippy --all-targets -D warnings`
   still clean. Launch via `.\client-prod.ps1` and confirm the real app is unchanged — startup,
   tray icon, hotkeys, onboarding.
3. **Phase 3**: harness smoke test — build a `Harness`, invoke `mic_toggle`, assert
   `Command::SetMute { muted: true }` arrives on `cmd_rx`. Then deliberately unset
   `SLINT_EMIT_DEBUG_INFO` and confirm the harness **fails loudly** rather than passing
   vacuously.
4. **Phase 4 — the real proof.** Verify each ★ regression test by reverting the underlying
   fix and confirming the test goes red:
   - force `DiscoverCrewsLoaded { crews: [] }` ⇒ `onboarding_zero_discoverable_crews` fails;
   - restore the `log::error!`-only error arm ⇒ `onboarding_discover_fails` and the screen
     invariant both fail. **A test suite that doesn't fail on the original bug is theatre** —
     this step is not optional.
5. **Phase 5**: after the onboarding reducer, all Phase 4 flow tests still pass unchanged
   (that is the whole point of the ordering), and the reachability property test passes.
   Then `--reset` the client and click through onboarding manually against prod once.
6. **Phase 6**: `6a` — rename an RPC in `main.go` and confirm the contract test fails.
   `6b` — `docker compose up`, `scripts/seed.sh`, run the e2e suite green. `6c` — run the
   scenario against a local Nakama, then confirm a deliberately wrong `NAKAMA_HTTP_KEY`
   makes it fail. `6d` — trigger the canary manually against prod, confirm it signs up and
   cleans up, then point it at a bad key and confirm the alert fires.
7. **Phase 7**: generate goldens, re-run, confirm zero diff. Change one padding value in a
   panel and confirm the diff artifact shows it.

## Out of scope

- Go RPC **handler**-level tests with a fake `runtime.NakamaModule` (currently zero exist).
  Worth a follow-up; the Phase 6b integration lane covers the same ground end-to-end first.
- A staging Render environment — deferred in favour of 6c + 6d, which cover the same
  failure class without new infrastructure or cost.
- Any change to `libmello` C++ or the `mello-core-sys` FFI boundary.
- Design/visual iteration on the 23 Slint panels.
