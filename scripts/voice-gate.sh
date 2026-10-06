#!/bin/sh
# Voice quality gate (plans/voice-quality.md stage 7).
#
# Pushes the speech corpus through the real libmello voice path for every
# impairment profile in benchmarks/baselines/voice/profiles.json, device-free,
# and prints the table with the delta against
# benchmarks/baselines/voice/baseline-2.9.json per profile and per metric.
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

exec cargo run --quiet -p voice-gate -- "$@"
