# Perf Harness Specification

> **Status:** v1 implemented (headless)  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md), [15-DEBUG-TELEMETRY.md](./15-DEBUG-TELEMETRY.md)

## 1. Purpose

Local, agent-runnable performance regression harness for the Mello desktop client. Runs headless `mello-core` scenarios, samples RSS/CPU, emits `perf-report.json`, and optionally compares to a committed baseline.

## 2. Entry point

```bash
./mello/scripts/perf/run.sh
./mello/scripts/perf/run.sh --write-baseline   # refresh benchmarks/baselines/macos-arm64.json
```

Direct:

```bash
cd mello && cargo run --release -p perf-harness -- run \
  --scenario-dir tools/perf-harness/scenarios \
  --output /tmp/perf-report.json \
  --compare benchmarks/baselines/macos-arm64.json
```

## 3. v1 scenarios (headless)

| ID | Requires backend | Notes |
|----|----------------|-------|
| `idle-connected` | No (device auth) | 30s sample after `DeviceAuthed` |
| `voice-muted` | Yes | login → join voice → mute → 45s sample |
| `voice-speaking` | Yes | + WAV inject loop → 45s sample |

Environment (voice scenarios):

- `PERF_TEST_EMAIL`, `PERF_TEST_PASSWORD`
- `PERF_TEST_CREW_ID`, `PERF_TEST_CHANNEL_ID`
- `PERF_TEST_WAV` — mono / 48 kHz / 16-bit PCM
- `NAKAMA_SERVER_KEY` — development key

## 4. Measurement rules

- **Build:** `cargo build --release -p perf-harness`
- **Logs:** `RUST_LOG=info` (not `client-dev.sh` debug firehose)
- **Do not** use `SLINT_EMIT_DEBUG_INFO` or `mcp` feature for baseline runs
- External sampler: `ps` RSS + CPU every 1s on harness PID
- In-process: `Event::StatsUpdated` (`MelloStats`) at 1 Hz from mello-core

## 5. Report schema

See `tools/perf-harness/src/report.rs`. Key outputs:

- `perf-report.json` — machine-readable
- `perf-report.md` — human summary
- `regressions.json` embedded in report when `--compare` used

Default tolerances: +10% RSS p95, +25% CPU p95 (or +2pp absolute).

## 6. Agent workflows

**Regression check after a PR:**

1. `backend/docker-compose up` (voice scenarios)
2. `./mello/scripts/perf/run.sh`
3. Read `scripts/perf/artifacts/*/perf-report.md`

**Exploratory UI (dev):** use `client-dev.sh` + Slint 1.17 MCP at `http://localhost:8765/mcp` — not part of regression baselines.

## 7. v2 (GUI client)

Full Slint `mello` binary — measures what users see in Activity Monitor.

```bash
./mello/scripts/perf/run-gui.sh
./mello/scripts/perf/run-gui.sh --write-baseline   # refresh benchmarks/baselines/macos-arm64-gui.json
```

Direct:

```bash
cargo build --release -p mello-client
cd mello && cargo run --release -p perf-harness -- run-gui \
  --scenario-dir tools/perf-harness/scenarios-gui \
  --output /tmp/perf-report-gui.json \
  --compare benchmarks/baselines/macos-arm64-gui.json
```

| ID | Requires backend | Notes |
|----|----------------|-------|
| `gui-idle` | Yes | login → select crew → 30s sample with window shown |

Client env (set by harness): `MELLO_PERF_MODE=1`, `MELLO_PERF_SCENARIO`, `MELLO_PERF_SIGNAL_DIR`.

Planned: `gui_voice_*`, `gui_chat_history`, `gui_stream_watch` after native video presenter.
