#!/usr/bin/env bash
# build.sh — Standalone CMake build for libmello (macOS / Linux)
# Usage: ./build.sh [--clean] [--release]

set -euo pipefail
cd "$(dirname "$0")"

BUILD_DIR="build"
VCPKG_ROOT="../external/vcpkg"
CONFIG="Debug"
CLEAN=false

for arg in "$@"; do
    case $arg in
        --clean)  CLEAN=true ;;
        --release) CONFIG="Release" ;;
    esac
done

# Detect triplet
if [[ "$(uname)" == "Darwin" ]]; then
    ARCH=$(uname -m)
    if [[ "$ARCH" == "arm64" ]]; then
        TRIPLET="arm64-osx"
    else
        TRIPLET="x64-osx"
    fi
else
    TRIPLET="x64-linux"
fi

# Bootstrap vcpkg if needed
if [[ ! -x "$VCPKG_ROOT/vcpkg" ]]; then
    echo "[build] Bootstrapping vcpkg..."
    "$VCPKG_ROOT/bootstrap-vcpkg.sh" -disableMetrics
fi

if $CLEAN && [[ -d "$BUILD_DIR" ]]; then
    echo "[build] Cleaning build directory..."
    rm -rf "$BUILD_DIR"
fi

mkdir -p "$BUILD_DIR"

echo "[build] Configuring ($CONFIG, triplet=$TRIPLET)..."
cmake -S . -B "$BUILD_DIR" \
    -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake" \
    -DVCPKG_TARGET_TRIPLET="$TRIPLET" \
    -DCMAKE_BUILD_TYPE="$CONFIG" \
    -DMELLO_BUILD_TESTS=OFF

echo "[build] Building..."
cmake --build "$BUILD_DIR" --config "$CONFIG" -j "$(nproc 2>/dev/null || sysctl -n hw.ncpu)"

echo "[build] Success! Output in $BUILD_DIR"
