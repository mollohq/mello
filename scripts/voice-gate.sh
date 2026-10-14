#!/bin/sh
# Voice quality gate (plans/voice-quality.md stage 7).
#
# Pushes the speech corpus through the real libmello voice path for every
# impairment profile in benchmarks/baselines/voice/profiles.json, device-free,
# and prints the table with the delta against the baseline per profile and
# per metric.
#
# Baseline and platform. The baseline is
# benchmarks/baselines/voice/baseline-2.9-slot-<os>-<arch>.json when it
# exists for this host, else baseline-2.9-slot.json. <os> and <arch> are
# Rust's names (windows, macos, linux; x86_64, aarch64). The gate enforces
# its numbers only on the platform the baseline file names in "platform".
# On another host it runs with --report-only: every metric and gate is
# printed, and only decode errors or a scorer failure fail the run.
# VOICE_GATE_PLATFORM=<os>/<arch> (uname words allowed, for example
# darwin/arm64) replaces the host platform, to test this rule.
#
# Exit code: 0 = pass, 1 = an enforced gate failed (a MOS drop of more than
# 0.1 on any profile, or an enforced structural gate), 2 = setup error.
#
# MOS scorer: PESQ wideband (ITU-T P.862.2) from the pinned `pesq` package in
# a venv under target/voice-gate/pyenv. The first run creates the venv (needs
# Python 3 with venv, network, and a C compiler for pesq). Without it the gate
# still runs and reports every structural metric, and the MOS gate is skipped
# with a warning. Set VOICE_GATE_PYTHON to use another interpreter.
#
# Usage:
#   ./scripts/voice-gate.sh                 # all profiles
#   ./scripts/voice-gate.sh --only clean,wrap
#   ./scripts/voice-gate.sh --no-score      # structural metrics only
#   ./scripts/voice-gate.sh --report-only   # print every gate, enforce none
# Audio, delay curves and results.json land in target/voice-gate/.
set -eu

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

# The gate needs no device. Keep CI=true like every other gate script.
export CI=true

VENV="$ROOT/target/voice-gate/pyenv"

venv_python() {
    if [ -x "$VENV/bin/python" ]; then
        printf '%s\n' "$VENV/bin/python"
    elif [ -x "$VENV/Scripts/python.exe" ]; then
        printf '%s\n' "$VENV/Scripts/python.exe"
    fi
}

if [ -z "${VOICE_GATE_PYTHON:-}" ]; then
    PY="$(venv_python)"
    if [ -z "$PY" ]; then
        SYS_PY=""
        for cand in python3 python; do
            # The Windows Store alias exists but fails; test that it runs.
            if command -v "$cand" >/dev/null 2>&1 && "$cand" -c 'import sys' >/dev/null 2>&1; then
                SYS_PY="$cand"
                break
            fi
        done
        if [ -n "$SYS_PY" ]; then
            printf 'voice-gate: creating the scorer venv in %s\n' "$VENV" >&2
            "$SYS_PY" -m venv "$VENV" >&2 || true
            PY="$(venv_python)"
        fi
    fi
    if [ -n "$PY" ] && ! "$PY" -c 'import pesq' >/dev/null 2>&1; then
        printf 'voice-gate: installing the pinned scorer (scripts/voice-gate-requirements.txt)\n' >&2
        "$PY" -m pip install --quiet --disable-pip-version-check \
            -r scripts/voice-gate-requirements.txt >&2 || true
    fi
    if [ -n "$PY" ]; then
        export VOICE_GATE_PYTHON="$PY"
    else
        printf 'voice-gate: WARNING: no python 3; structural metrics only\n' >&2
    fi
fi

# Rust's names for a platform: windows, macos, linux; x86_64, aarch64.
norm_os() {
    case "$1" in
        MINGW* | MSYS* | CYGWIN* | Windows* | windows) printf 'windows\n' ;;
        Darwin | darwin | macos) printf 'macos\n' ;;
        Linux | linux) printf 'linux\n' ;;
        *) printf '%s\n' "$1" | tr 'A-Z' 'a-z' ;;
    esac
}
norm_arch() {
    case "$1" in
        x86_64 | amd64 | AMD64 | x86-64) printf 'x86_64\n' ;;
        arm64 | aarch64 | ARM64) printf 'aarch64\n' ;;
        *) printf '%s\n' "$1" ;;
    esac
}

if [ -n "${VOICE_GATE_PLATFORM:-}" ]; then
    HOST_OS="$(norm_os "${VOICE_GATE_PLATFORM%%/*}")"
    HOST_ARCH="$(norm_arch "${VOICE_GATE_PLATFORM#*/}")"
else
    HOST_OS="$(norm_os "$(uname -s)")"
    HOST_ARCH="$(norm_arch "$(uname -m)")"
fi

BASE_DIR="benchmarks/baselines/voice"
BASELINE="$BASE_DIR/baseline-2.9-slot-$HOST_OS-$HOST_ARCH.json"
if [ ! -f "$BASELINE" ]; then
    BASELINE="$BASE_DIR/baseline-2.9-slot.json"
fi

# The platform the baseline was taken on: its "platform" object, which the
# harness writes as "arch" and "os" lines (serde_json, sorted keys).
baseline_platform_field() {
    awk -v key="\"$2\":" '
        /"platform": \{/ { inside = 1; next }
        inside && /\}/ { exit }
        inside && index($0, key) {
            sub(/^[^:]*: *"/, ""); sub(/".*$/, ""); print; exit
        }' "$1"
}

REPORT_ONLY=""
if [ -f "$BASELINE" ]; then
    BASE_OS="$(baseline_platform_field "$BASELINE" os)"
    BASE_ARCH="$(baseline_platform_field "$BASELINE" arch)"
    if [ "$BASE_OS/$BASE_ARCH" != "$HOST_OS/$HOST_ARCH" ]; then
        printf 'voice-gate: %s is from %s/%s, this host is %s/%s: report only\n' \
            "$BASELINE" "$BASE_OS" "$BASE_ARCH" "$HOST_OS" "$HOST_ARCH" >&2
        REPORT_ONLY="--report-only"
    fi
fi

# $REPORT_ONLY is empty or one word: unquoted on purpose.
# shellcheck disable=SC2086
exec cargo run --quiet -p voice-gate -- --baseline "$(basename "$BASELINE")" $REPORT_ONLY "$@"
