#!/bin/sh
# Streaming quality gate (plans/streaming-quality.md stage 1).
#
# Plays the deterministic reference clip through the real stream path: the
# reference source, the preprocessor and the encoder, the RTP sender, an
# impairment shim, the real SFU in test mode, the shim again, the RTP
# receiver session, the decoder and the present. Runs every profile of
# benchmarks/baselines/stream/profiles.json and prints the table with the
# delta against the baseline per profile and per metric.
#
# Lanes:
#   --lane cpu (default)  720p60, OpenH264 encode and decode in software
#                         (MELLO_SOFT_ENCODER=1, MELLO_SOFT_DECODER=1)
#   --lane hw             1080p60, the machine's hardware encoder and decoder
# Baseline: benchmarks/baselines/stream/baseline-2.1-<lane>.json.
#
# Exit code: 0 = pass, 1 = an enforced gate failed (VMAF down by more than 1
# point or freeze time up by more than its spread on any profile, or a gate
# of gates.json), 2 = setup error.
#
# Needs: Go (the SFU is built from MELLO_SFU_DIR, default ../mello-sfu next
# to this repository), Python 3 (standard library only), network on the
# first run for two pinned downloads:
#   - the Cisco OpenH264 2.6.0 DLL (the CPU lane encoder and decoder), into
#     libmello/third_party/openh264/ (git-ignored; see its README.md)
#   - the vmaf 3.0.0 CLI (Netflix release), into target/stream-gate/tools/
# Both are checked against the SHA-256 below before use. UDP port 10000 and
# the address 127.0.0.2 must be free (stop a local SFU first).
#
# Usage:
#   ./scripts/stream-gate.sh                       # CPU lane, all profiles
#   ./scripts/stream-gate.sh --lane hw             # hardware lane
#   ./scripts/stream-gate.sh --only clean,mobile   # some profiles
#   ./scripts/stream-gate.sh --no-vmaf             # PSNR and SSIM only
#   ./scripts/stream-gate.sh --repeat 3            # spread of every metric
#   ./scripts/stream-gate.sh --report-only         # print every gate, enforce none
# Output (results.json, per-profile presents.csv, host.csv, wire.csv, the
# SFU log) lands in target/stream-gate/<lane>/.
set -eu

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
export CI=true

OPENH264_DIR="$ROOT/libmello/third_party/openh264"
OPENH264_DLL="openh264-2.6.0-win64.dll"
OPENH264_URL="https://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2"
OPENH264_BZ2_SHA256="dab5f2a872777f9a58b69bfa9fbcf20d9f82f2d6ec91383fd70bff49bd34ac9f"
OPENH264_DLL_SHA256="2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691"
VMAF_VERSION="3.0.0"
VMAF_URL="https://github.com/Netflix/vmaf/releases/download/v3.0.0/vmaf.exe"
VMAF_SHA256="b7fcb257a3c26b0c8071409df95219e1e4f569b99fde0492521caf1826e95d48"
TOOLS="$ROOT/target/stream-gate/tools"

PY=""
for cand in python3 python; do
    # The Windows Store alias exists but fails; test that it runs.
    if command -v "$cand" >/dev/null 2>&1 && "$cand" -c 'import sys' >/dev/null 2>&1; then
        PY="$cand"
        break
    fi
done
if [ -z "$PY" ]; then
    printf 'stream-gate: Python 3 is required (hash checks and the VMAF scorer)\n' >&2
    exit 2
fi
export STREAM_GATE_PYTHON="$(command -v "$PY")"

sha256() {
    "$PY" -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
}

fetch() {
    # fetch URL DEST SHA256
    tmp="$2.part"
    curl -sSfL -o "$tmp" "$1" || { rm -f "$tmp"; return 1; }
    got="$(sha256 "$tmp")"
    if [ "$got" != "$3" ]; then
        rm -f "$tmp"
        printf 'stream-gate: %s has SHA-256 %s, expected %s\n' "$1" "$got" "$3" >&2
        return 1
    fi
    mv "$tmp" "$2"
}

case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
        if [ ! -f "$OPENH264_DIR/$OPENH264_DLL" ]; then
            printf 'stream-gate: fetching %s\n' "$OPENH264_URL" >&2
            fetch "$OPENH264_URL" "$OPENH264_DIR/$OPENH264_DLL.bz2" "$OPENH264_BZ2_SHA256" || exit 2
            "$PY" -c 'import bz2,sys; open(sys.argv[2],"wb").write(bz2.decompress(open(sys.argv[1],"rb").read()))' \
                "$OPENH264_DIR/$OPENH264_DLL.bz2" "$OPENH264_DIR/$OPENH264_DLL"
            rm -f "$OPENH264_DIR/$OPENH264_DLL.bz2"
        fi
        if [ "$(sha256 "$OPENH264_DIR/$OPENH264_DLL")" != "$OPENH264_DLL_SHA256" ]; then
            printf 'stream-gate: %s does not match the pinned SHA-256; delete it to fetch it again\n' \
                "$OPENH264_DIR/$OPENH264_DLL" >&2
            exit 2
        fi
        VMAF_BIN="$TOOLS/vmaf-$VMAF_VERSION/vmaf.exe"
        if [ -z "${STREAM_GATE_VMAF:-}" ]; then
            if [ ! -f "$VMAF_BIN" ]; then
                mkdir -p "$(dirname "$VMAF_BIN")"
                printf 'stream-gate: fetching %s\n' "$VMAF_URL" >&2
                fetch "$VMAF_URL" "$VMAF_BIN" "$VMAF_SHA256" ||
                    printf 'stream-gate: WARNING: no vmaf CLI; PSNR and SSIM only\n' >&2
            fi
            if [ -f "$VMAF_BIN" ] && [ "$(sha256 "$VMAF_BIN")" = "$VMAF_SHA256" ]; then
                export STREAM_GATE_VMAF="$VMAF_BIN"
            fi
        fi
        ;;
    *)
        # The reference source is Windows only (D3D11). Other platforms can
        # run the unit tests of tools/stream-gate, not the gate.
        printf 'stream-gate: the gate runs on Windows only (the reference source is D3D11)\n' >&2
        exit 2
        ;;
esac

if [ -z "${MELLO_SFU_DIR:-}" ]; then
    export MELLO_SFU_DIR="$ROOT/../mello-sfu"
fi

# A release build: the gate measures the timing of the host manager, the
# pacer and the viewer tick, which ship optimized.
cargo build --release --quiet -p stream-gate
# build.rs copies the DLLs to target/release; the harness runs from there.
cp -f "$OPENH264_DIR/$OPENH264_DLL" "$ROOT/target/release/" 2>/dev/null || true
exec "$ROOT/target/release/stream-gate" "$@"
