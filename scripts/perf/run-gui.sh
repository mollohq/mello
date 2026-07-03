#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MELLO_REPO="${MELLO_REPO:-$(cd "${SCRIPT_DIR}/../.." && pwd)}"
WORKSPACE_ROOT="$(cd "${MELLO_REPO}/.." && pwd)"
FIXTURE_ENV_PATH="${FIXTURE_ENV_PATH:-${WORKSPACE_ROOT}/scripts/voice/.generated/local-fixtures.env}"
ARTIFACT_ROOT="${ARTIFACT_ROOT:-${SCRIPT_DIR}/artifacts}"
SCENARIO_DIR="${SCENARIO_DIR:-${MELLO_REPO}/tools/perf-harness/scenarios-gui}"
BASELINE="${BASELINE:-${MELLO_REPO}/benchmarks/baselines/macos-arm64-gui.json}"
WRITE_BASELINE=0
OUTPUT=""

usage() {
    cat <<'EOF'
Usage: run-gui.sh [--write-baseline] [--baseline PATH] [--output PATH]

Runs full Slint client perf scenarios (release build) and compares to GUI baseline.

Requires local Nakama (backend/docker-compose up). Auto-loads voice fixtures from
scripts/voice/.generated/local-fixtures.env when present (run prepare-fixtures.sh first).

Examples:
  ./scripts/perf/run-gui.sh
  ./scripts/perf/run-gui.sh --write-baseline
EOF
}

load_perf_fixtures() {
    if [[ -f "$FIXTURE_ENV_PATH" ]]; then
        # shellcheck disable=SC1090
        source "$FIXTURE_ENV_PATH"
        echo "[perf-gui] loaded fixtures from $FIXTURE_ENV_PATH"
    fi

    export PERF_TEST_EMAIL="${PERF_TEST_EMAIL:-${VOICE_TEST_EMAIL:-}}"
    export PERF_TEST_PASSWORD="${PERF_TEST_PASSWORD:-${VOICE_TEST_PASSWORD:-}}"
    export PERF_TEST_CREW_ID="${PERF_TEST_CREW_ID:-${VOICE_TEST_CREW_ID:-}}"
    export PERF_TEST_CHANNEL_ID="${PERF_TEST_CHANNEL_ID:-${VOICE_TEST_CHANNEL_ID:-}}"

    if [[ -z "${PERF_TEST_EMAIL}" ]]; then
        echo "[perf-gui] PERF_TEST_EMAIL not set." >&2
        echo "[perf-gui] Run: ${WORKSPACE_ROOT}/scripts/voice/prepare-fixtures.sh" >&2
        echo "[perf-gui] Or export PERF_TEST_EMAIL / PERF_TEST_PASSWORD / PERF_TEST_CREW_ID." >&2
        exit 2
    fi
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --write-baseline) WRITE_BASELINE=1; shift ;;
        --baseline) BASELINE="$2"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) echo "unknown flag: $1" >&2; usage >&2; exit 2 ;;
    esac
done

load_perf_fixtures

TIMESTAMP="$(date -u +"%Y%m%dT%H%M%SZ")"
RUN_DIR="${ARTIFACT_ROOT}/${TIMESTAMP}-perf-gui-$$"
mkdir -p "$RUN_DIR"

if [[ -z "$OUTPUT" ]]; then
    OUTPUT="${RUN_DIR}/perf-report-gui.json"
fi

export RUST_LOG="${RUST_LOG:-info}"
export CI="${CI:-true}"
export NAKAMA_SERVER_KEY="${NAKAMA_SERVER_KEY:-mello_dev_key}"

cd "${MELLO_REPO}"
cargo build --release -p mello-client -p perf-harness

MELLO_BIN="${MELLO_BIN:-${MELLO_REPO}/target/release/mello}"
export MELLO_BIN

HARNESS_ARGS=(run-gui --scenario-dir "$SCENARIO_DIR" --output "$OUTPUT")
if [[ "$WRITE_BASELINE" -eq 0 && -f "$BASELINE" ]]; then
    HARNESS_ARGS+=(--compare "$BASELINE")
fi

cargo run --release -p perf-harness --quiet -- "${HARNESS_ARGS[@]}"

if [[ "$WRITE_BASELINE" -eq 1 ]]; then
    cargo run --release -p perf-harness --quiet -- write-baseline \
        --report "$OUTPUT" \
        --output "$BASELINE"
    echo "[perf-gui] baseline written to $BASELINE"
fi

echo "[perf-gui] report: $OUTPUT"
if [[ -f "${OUTPUT%.json}.md" ]]; then
    echo "[perf-gui] summary: ${OUTPUT%.json}.md"
fi
