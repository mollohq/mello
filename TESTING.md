# Testing & Harnesses

One place that lists every test tool across the three repos (`mello`, `mello-sfu`,
`mello-site`), how to run it, and when to reach for it. Skim the quick reference,
then jump to the section you need.

> Repos: this file lives in `mello/`. The SFU (`mello-sfu`) and marketing/test
> site (`mello-site`) are sibling repos checked out next to `mello/`.

---

## Quick reference

| Tool / suite | Layer | Needs a backend? | Runs in PR CI? | Command (short) |
|---|---|---|---|---|
| `cargo test --workspace` | Rust (core, client, tools) | No | ✅ yes | `cargo test --workspace` |
| `cargo fmt` / `cargo clippy` | Rust | No | ✅ yes | `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings` |
| libmello `ctest` | C++ audio/video DSP | No | ⚠️ not wired (run locally) | `cd libmello && cmake --build build && ctest --test-dir build` |
| Nakama `go test` | Backend modules | No | ⚠️ not wired (run locally) | `cd backend/nakama/data/modules && go test ./...` |
| `dev_fault` RPC | Backend fault injection | Yes (live Nakama) | No (manual/integration) | RPC call, gated by `MELLO_ENABLE_DEV_FAULT=1` |
| SFU `go test` | SFU server | No | ⚠️ not wired (run locally) | `cd mello-sfu && go test ./...` |
| `voice-soak` | SFU load/churn/resilience | Yes (live SFU) | No (manual/integration) | `go run ./tools/voice-soak ...` |
| `stream-soak` | SFU stream relay | Yes (live SFU) | No (manual/integration) | `go run ./tools/stream-soak ...` |
| `voice-test-client` (GUI) | Client DSP A/B | Yes (live backend) | No (manual) | `cargo run` in `tools/voice-test-client` |
| `voice-test-client` (headless) | Client reconnect/resync E2E | Yes (live backend) | 🔧 integration job | `cargo run -- --scenario scenarios/<f>.json` |
| `scripts/voice/voice-local-gate.sh` | Cross-repo local RED/GREEN gate + artifacts | Yes (local Nakama + SFU) | 🔧 integration job | `../scripts/voice/voice-local-gate.sh` |
| `sfu-test.html` | Browser voice/stream + robustness | Yes (live SFU) | No (manual) | open via `npm run dev` in `mello-site` |

Legend: ✅ runs today · ⚠️ runnable, not yet in the PR workflow · 🔧 needs an
integration job with a seeded backend.

---

## What PR CI runs today

`.github/workflows/pr-checks.yml` runs on self-hosted **Windows** and **macOS**
runners for every PR to `main`:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings   # warnings are errors
cargo test --workspace
```

Notes:

- **`CI=true` is set automatically by GitHub Actions.** That's what makes
  `cargo test --workspace` skip the device-dependent audio/video tests. Locally
  those tests block waiting on real capture/playback devices, so run local tests
  with `CI=true` too (PowerShell: `$env:CI='true'; cargo test --workspace`).
- `--all-targets` + `--workspace` now also lint/build `tools/voice-test-client`
  (it's a workspace member). Keep it clippy-clean or the PR lane breaks.
- The Go (backend + SFU) and C++ (libmello) suites are **not** in the PR
  workflow yet — run them locally before "done" (see CLAUDE.md), or add jobs.

---

## Rust — core, client, tools

From the `mello/` repo root:

```bash
cargo fmt --all                 # format
cargo clippy --all-targets -- -D warnings
$env:CI='true'; cargo test --workspace      # PowerShell; bash: CI=true cargo test --workspace
```

Useful filters:

```bash
cargo test -p mello-core --lib reconnect      # reconnect supervisor unit tests
cargo test -p mello-core --lib command::tests
cargo test -p mello-core --lib events::tests
```

Highlights of what's covered:

- **`mello-core/src/client/reconnect.rs`** — `ReconnectSupervisor` is pure
  decision logic (backoff, wake-gap detection, lost/recovered edges) with
  deterministic unit tests using injected clocks. This is the regression net for
  sleep/wake + reconnect behaviour; no backend required.
- **`events.rs` / `command.rs`** — lock the FFI JSON shape (adjacently-tagged
  `{"type","data"}`) used across the core↔client boundary.

---

## libmello — C++ audio/video DSP

Unit tests live in `libmello/tests/` (echo canceller, VAD, Opus, jitter buffer,
noise suppressor, video pipeline). Per CLAUDE.md:

```bash
cd libmello
cmake -B build -S .          # first time / after CMake changes
cmake --build build
ctest --test-dir build --output-on-failure
```

Run when you touch anything under `libmello/src/audio/` or `libmello/src/video/`
(read `specs/03-LIBMELLO.md` first — threading/COM/callback invariants).

---

## Backend — Nakama Go modules

Unit tests (no server needed) live next to the code in
`backend/nakama/data/modules/`:

```bash
cd backend/nakama/data/modules
go vet ./...
go test ./...
```

> The modules are a Nakama **plugin**, not a standalone binary — `go build` of
> `main` will fail; use `go vet` / `go test` for static + unit checks.

Covered: idempotent voice-join, `cleanupVoiceOnCrewExit` guards, voice-update
push-drop scheduling, reconcile-oracle helpers.

### Local full stack

For anything touching live RPCs/hooks, bring up the Docker stack:

```bash
cd backend
docker-compose up
```

### `dev_fault` RPC (fault injection)

`DevFaultRPC` deterministically injects voice-state drift so the reconcile
oracle / GC / client-resync paths can be exercised against a live server. It is
**gated** — it mutates real voice state and can evict real users:

```bash
# Enable on the Nakama instance:
MELLO_ENABLE_DEV_FAULT=1
```

Actions (payload `{"action": ...}`):

- `ghost_member` — inject a member Nakama thinks is present but the SFU isn't
  (backdated so the reconcile oracle prunes it next tick). Requires
  `channel_id`, `crew_id`, `user_id`.
- `force_leave` — simulate a missed leave for `user_id`.
- `clear_channel` — evict every member of `channel_id`.
- `drop_next_push` — drop the next `count` `voice_update` pushes for `crew_id`
  (simulates lost notifications → client must self-heal).

Never enable in production.

---

## SFU — `mello-sfu`

### Unit tests

```bash
cd mello-sfu
gofmt -l .          # formatting (should print nothing)
go test ./...
```

### Run the SFU locally

```bash
cd mello-sfu
SFU_ADMIN_PASSWORD=devpass go run ./cmd/sfu
# health:  http://localhost:8080/health
# admin:   http://localhost:8080/admin
```

### `voice-soak` — voice load / churn / resilience

Drives synthetic clients through connect→join→(WebRTC)→hold→leave against a live
SFU in test mode (`SFU_ADMIN_PASSWORD` must be set). Exit code is non-zero when
gates fail, so it's CI/script friendly.

Smoke / churn:

```bash
SFU_ADMIN_PASSWORD=devpass \
go run ./tools/voice-soak \
  --endpoint ws://127.0.0.1:8443/ws --admin-password devpass \
  --clients 30 --rounds 40 --hold-ms 400
```

Full WebRTC lifecycle + RTP load: add `--negotiate-webrtc --rtp-audio`.
Capacity sweep / auto-search: `--sweep-clients 25,50,75,100` or
`--auto-capacity-search`. (Full flag list in `mello-sfu/README.md`.)

Resilience flags added for the robustness work:

- `--reuse-user-id` — stable SFU identity per client across rounds, exercising
  the **same-user reconnect / eviction** path.
- `--half-open` (+ `--half-open-hold-ms`) — after join, freeze the client (stop
  reading WS, keep the PeerConnection alive) to simulate a **sleep/wake zombie**.
- `--idle-resume-ms` — idle (no RTP) for a while after connect, then resume
  sending RTP (requires `--negotiate-webrtc`).

### `stream-soak` — stream relay / impairment matrix

Same idea for the stream path (1 host + N viewers, DataChannel media). Profiles:
`goodNetwork`, `typicalHome`, `roughHome`. See `mello-sfu/README.md` and
`tools/stream-soak/run-1080-gate.sh`.

---

## Client — `voice-test-client`

Lives at `mello/tools/voice-test-client/`. Two modes share one `mello-core`
path.

### GUI mode (DSP A/B)

```bash
cd tools/voice-test-client
cargo run                       # dev backend
VOICE_TEST_PRODUCTION=1 cargo run   # prod backend (also set NAKAMA_SERVER_KEY / NAKAMA_HTTP_KEY)
```

Login → select crew/channel → join voice → inject WAV → switch NS mode → read
live telemetry. WAV files must be **mono / 48 kHz / 16-bit PCM**
(`bash scripts/fetch_dataset.sh` bootstraps `test-data/`). See the tool's
`README.md` for the A/B protocol and MOS template.

### Headless scenario mode (reconnect/resync E2E)

Drives a real client (no UI) through a JSON scenario and asserts events — this is
the end-to-end net for the sleep/wake + reconnect + resync work.

```bash
cd tools/voice-test-client
cargo run -- --scenario scenarios/smoke.json
# or: VOICE_TEST_SCENARIO=scenarios/reconnect.json cargo run
```

Scenario steps: `device_auth`, `login`, `select_crew`, `join_voice`,
`leave_voice`, `inject_wav`, `stop_inject`, `set_mute`, `sleep`, `fault`,
`expect_event`, `assert_no_event`. `${ENV}` tokens in the JSON are expanded from
the environment, so one file parameterises per run/CI.

Fault kinds (compiled in via the `test-faults` feature, which this tool enables —
they are **never** in production/FFI builds):

- `nakama_disconnect` — force the realtime WS down → reconnect path.
- `sfu_disconnect` — force the voice session disconnected → voice reconnect.
- `simulate_suspend` — backdate the liveness clock → wake-gap detection.

Bundled scenarios in `scenarios/`:

- **`smoke.json`** — `device_auth` → expect `DeviceAuthed`. Turnkey: proves the
  binary builds (with `test-faults`), reaches the backend, and authenticates. No
  crew/channel/WAV fixtures needed. Good first gate for an integration job.
- **`reconnect.json`** — login → join voice → inject audio → `nakama_disconnect`
  → expect `ConnectionStateChanged` (loss detected) → expect a second
  `VoiceJoined` (resync re-joined voice = **recovery proven**) plus stability
  windows (`assert_no_event VoiceSfuDisconnected`) before/after fault injection
  → leave.
  Parameterised via env:

  | Var | Meaning |
  |---|---|
  | `VOICE_TEST_EMAIL` / `VOICE_TEST_PASSWORD` | seeded test account |
  | `VOICE_TEST_CREW_ID` | crew the account belongs to |
  | `VOICE_TEST_CHANNEL_ID` | a voice channel in that crew |
  | `VOICE_TEST_WAV` | path to a mono/48k/16-bit WAV |

  Backend host via `NAKAMA_HOST` / `NAKAMA_PORT` / `NAKAMA_SSL`
  (`VOICE_TEST_PRODUCTION=1` for the prod profile).

The process exits `0` on `PASS`, non-zero on `FAIL` (first unmet `expect_event`),
so scenarios gate a CI job directly.

### External local gate (`../scripts/voice`)

For end-to-end local stack validation across `mello` + `mello-sfu`, use the
workspace-level harness:

```bash
../scripts/voice/voice-local-up.sh
unset CARGO_TARGET_DIR
../scripts/voice/voice-local-gate.sh --skip-up --skip-fixtures
```

Key guards baked into the gate:

- Fails on SFU liveness regression markers (`liveness_timeout`, repeated unhealthy checks).
- Fails if voice scenarios silently fell back to P2P (`SFU peer creation failed ... falling back to P2P`).
- Collects per-step artifacts (`command.log`, Nakama/SFU logs, health/overview API snapshots).

Desktop + mobile LAN testing:

- `voice-local-up.sh` now auto-sets `SFU_ENDPOINT_EU=ws://<lan-ip>:8443/ws`.
- It also sets `SFU_PUBLIC_IP=<lan-ip>` for ICE host candidates.
- Verify startup output is non-loopback before joining from mobile.

---

## Browser — `sfu-test.html`

A manual voice/stream console in `mello-site/`. Serve the static site and open
the page:

```bash
cd mello-site
npm run dev        # wrangler pages dev on :8788
# open http://localhost:8788/sfu-test.html
```

Connect to an SFU endpoint with a token, then drive voice/stream by hand and
watch live WebRTC stats. The **Robustness Tools** card adds:

- a reconnect loop (repeated disconnect→reconnect with latency measurement),
- a one-shot half-open trigger (drop the WS, keep the PeerConnection),
- threshold alerts (max RTT / jitter / loss) logged when stats exceed limits,
- log export.

Use it for quick interactive repros from a real browser when a soak run flags
something or you want eyes-on stats.

---

## When to run what

- **Every change / before pushing:** `cargo fmt` + `cargo clippy -D warnings` +
  `cargo test --workspace`. If you touched C++: `ctest`. If you touched backend
  or SFU Go: `go test ./...`.
- **Voice state / reconnect / presence logic:** add/extend a `reconnect.rs` unit
  test, then run `scenarios/reconnect.json` against the local stack with a
  `dev_fault` (`drop_next_push` / `ghost_member`) to prove self-healing.
- **SFU capacity / churn / sleep-wake zombies:** `voice-soak` (+ `--half-open`,
  `--reuse-user-id`, `--idle-resume-ms`).
- **Stream relay quality:** `stream-soak` profiles / 1080 gate.
- **Audio DSP quality (A/B, NS modes, MOS):** `voice-test-client` GUI.
- **Interactive browser repro / live stats:** `sfu-test.html`.

## Suggested CI lanes

1. **PR (fast, no backend) — exists:** Rust fmt/clippy/test. Add Go
   `go test ./...` (backend + SFU) and libmello `ctest` here — all are
   backend-free and fast.
2. **Integration (on demand / nightly):** `docker-compose up` the backend +
   `go run ./cmd/sfu`, seed a test account/crew/channel, then run
   `scenarios/smoke.json` (must pass) and `scenarios/reconnect.json`, plus a
   short `voice-soak` churn gate. These need live services and are too heavy/
   flaky for the per-PR lane.
