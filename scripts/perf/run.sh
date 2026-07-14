#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MELLO_REPO="${MELLO_REPO:-$(cd "${SCRIPT_DIR}/../.." && pwd)}"
ARTIFACT_ROOT="${ARTIFACT_ROOT:-${SCRIPT_DIR}/artifacts}"
SCENARIO_DIR="${SCENARIO_DIR:-${MELLO_REPO}/tools/perf-harness/scenarios}"
BASELINE="${BASELINE:-${MELLO_REPO}/benchmarks/baselines/macos-arm64.json}"
WRITE_BASELINE=0
OUTPUT=""
USE_CAPTURE=1

usage() {
    cat <<'EOF'
Usage: run.sh [--write-baseline] [--baseline PATH] [--output PATH] [--no-capture]

Runs headless perf scenarios (release perf-harness) and compares to baseline.

Requires local Nakama (backend/docker-compose up) for voice scenarios.
Set PERF_TEST_EMAIL, PERF_TEST_PASSWORD, PERF_TEST_CREW_ID, PERF_TEST_CHANNEL_ID,
and PERF_TEST_WAV (mono 48kHz 16-bit PCM) — same fixtures as scripts/voice.

Examples:
  ./scripts/perf/run.sh
  ./scripts/perf/run.sh --write-baseline
  ./scripts/perf/run.sh --baseline benchmarks/baselines/macos-arm64.json
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --write-baseline) WRITE_BASELINE=1; shift ;;
        --baseline) BASELINE="$2"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        --no-capture) USE_CAPTURE=0; shift ;;
        --help|-h) usage; exit 0 ;;
        *) echo "unknown flag: $1" >&2; usage >&2; exit 2 ;;
    esac
done

TIMESTAMP="$(date -u +"%Y%m%dT%H%M%SZ")"
RUN_DIR="${ARTIFACT_ROOT}/${TIMESTAMP}-perf-$$"
mkdir -p "$RUN_DIR"

if [[ -z "$OUTPUT" ]]; then
    OUTPUT="${RUN_DIR}/perf-report.json"
fi

export RUST_LOG="${RUST_LOG:-info}"
export CI="${CI:-true}"
export NAKAMA_SERVER_KEY="${NAKAMA_SERVER_KEY:-mello_dev_key}"

cd "${MELLO_REPO}"
cargo build --release -p perf-harness

run_harness() {
    cargo run --release -p perf-harness --quiet -- "$@"
}

HARNESS_ARGS=(run --scenario-dir "$SCENARIO_DIR" --output "$OUTPUT")
if [[ "$WRITE_BASELINE" -eq 0 && -f "$BASELINE" ]]; then
    HARNESS_ARGS+=(--compare "$BASELINE")
fi

if [[ "$USE_CAPTURE" -eq 1 && -x "${MELLO_REPO}/../scripts/voice/capture-run.sh" ]]; then
    "${MELLO_REPO}/../scripts/voice/capture-run.sh" --name perf-harness -- \
        env MELLO_REPO="$MELLO_REPO" RUST_LOG="$RUST_LOG" CI="$CI" NAKAMA_SERVER_KEY="$NAKAMA_SERVER_KEY" \
        bash -lc "cd \"${MELLO_REPO}\" && cargo run --release -p perf-harness --quiet -- $(printf '%q ' "${HARNESS_ARGS[@]}")"
else
    run_harness "${HARNESS_ARGS[@]}"
fi

if [[ "$WRITE_BASELINE" -eq 1 ]]; then
    run_harness write-baseline \
        --report "$OUTPUT" \
        --output "$BASELINE"
    echo "[perf] baseline written to $BASELINE"
fi

echo "[perf] report: $OUTPUT"
if [[ -f "${OUTPUT%.json}.md" ]]; then
    echo "[perf] summary: ${OUTPUT%.json}.md"
fi
