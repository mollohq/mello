#!/usr/bin/env bash
# Build the Linux client and pack it as a Velopack AppImage.
# Runs inside the build container (scripts/linux-package.sh calls it) and in
# the build-linux job of .github/workflows/release.yml.
#
# Usage: scripts/linux/package.sh VERSION [CHANNEL]
#   FEATURES: cargo features for mello-client. Empty (the default) is the
#   development build, which talks to Nakama on 127.0.0.1 unless NAKAMA_HOST
#   is set at run time. The release workflow builds `production`.
#   SKIP_BUILD=1: pack target/release/mello as it is (the release job built
#   and tested it already).
#   OUT_DIR: where vpk writes (default /dist, the container's dist-linux/).
#   Earlier releases of the channel in OUT_DIR become delta packages.
set -euo pipefail

VERSION=${1:?usage: package.sh VERSION [CHANNEL]}
CHANNEL=${2:-linux-x64-stable}
FEATURES=${FEATURES:-}
OUT_DIR=${OUT_DIR:-/dist}

if [ "${SKIP_BUILD:-0}" != "1" ]; then
    feature_args=()
    [ -n "$FEATURES" ] && feature_args=(--features "mello-client/$FEATURES")
    cargo build --release --locked -p mello-client ${feature_args[@]+"${feature_args[@]}"}
fi

STAGE=/tmp/m3llo-pack
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp target/release/mello "$STAGE/"
# Keep the symlinks: the binary needs libonnxruntime.so.1 (the soname).
cp -P libmello/third_party/onnxruntime/onnxruntime-linux-x64-*/lib/libonnxruntime.so* "$STAGE/"
cp libmello/models/silero_vad.onnx libmello/models/echo_suppressor.onnx "$STAGE/"

# Every shared library must resolve from the staged folder ($ORIGIN) or the
# system. A missing one would only show on the tester's machine.
if ldd "$STAGE/mello" | grep -q "not found"; then
    ldd "$STAGE/mello" | grep "not found"
    echo "package.sh: unresolved shared libraries" >&2
    exit 1
fi

echo "--- staged files"
ls -la "$STAGE"
echo "--- highest glibc symbol version the binary needs"
objdump -T "$STAGE/mello" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1

vpk pack \
    --packId m3llo \
    --packVersion "$VERSION" \
    --packDir "$STAGE" \
    --mainExe mello \
    --packTitle m3llo \
    --channel "$CHANNEL" \
    --icon client/assets/icons/mello.png \
    --outputDir "$OUT_DIR"

echo "--- output"
ls -la "$OUT_DIR"
