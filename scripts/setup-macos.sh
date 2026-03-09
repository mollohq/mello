#!/bin/bash
set -euo pipefail

# Setup script for macOS arm64 development
# Downloads third-party dependencies that are gitignored
# vcpkg deps are handled automatically in manifest mode via cmake

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
LIBMELLO_DIR="$ROOT_DIR/libmello"
THIRD_PARTY="$LIBMELLO_DIR/third_party"

echo "=== Mello macOS arm64 Setup ==="
echo "Root: $ROOT_DIR"

# --- vcpkg submodule ---
VCPKG_DIR="$ROOT_DIR/external/vcpkg"
if [ ! -f "$VCPKG_DIR/bootstrap-vcpkg.sh" ]; then
    echo ""
    echo "--- Initializing vcpkg submodule ---"
    cd "$ROOT_DIR"
    git submodule update --init external/vcpkg
fi

if [ ! -f "$VCPKG_DIR/vcpkg" ]; then
    echo ""
    echo "--- Bootstrapping vcpkg ---"
    "$VCPKG_DIR/bootstrap-vcpkg.sh" -disableMetrics
else
    echo "vcpkg already bootstrapped, skipping"
fi

# --- rnnoise ---
RNNOISE_DIR="$THIRD_PARTY/rnnoise"
if [ ! -d "$RNNOISE_DIR/src" ]; then
    echo ""
    echo "--- Cloning rnnoise ---"
    mkdir -p "$THIRD_PARTY"
    git clone --depth 1 https://github.com/xiph/rnnoise.git "$RNNOISE_DIR"
fi

# Download rnnoise model data (generates rnnoise_data.h/c)
if [ ! -f "$RNNOISE_DIR/src/rnnoise_data.h" ]; then
    echo "--- Downloading rnnoise model data ---"
    cd "$RNNOISE_DIR"
    bash download_model.sh
    cd "$ROOT_DIR"
else
    echo "rnnoise model data already present, skipping"
fi

# --- ONNX Runtime (macOS arm64) ---
ORT_VERSION="1.23.0"
ORT_DIR_NAME="onnxruntime-osx-arm64-${ORT_VERSION}"
ORT_DIR="$THIRD_PARTY/onnxruntime/$ORT_DIR_NAME"
if [ ! -d "$ORT_DIR/lib" ]; then
    echo ""
    echo "--- Downloading ONNX Runtime ${ORT_VERSION} (macOS arm64) ---"
    mkdir -p "$THIRD_PARTY/onnxruntime"
    ORT_URL="https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/${ORT_DIR_NAME}.tgz"
    echo "URL: $ORT_URL"
    curl -L "$ORT_URL" -o "/tmp/${ORT_DIR_NAME}.tgz"
    tar xzf "/tmp/${ORT_DIR_NAME}.tgz" -C "$THIRD_PARTY/onnxruntime/"
    rm "/tmp/${ORT_DIR_NAME}.tgz"
    echo "Extracted to: $ORT_DIR"
else
    echo "ONNX Runtime already present, skipping"
fi

# --- Silero VAD model ---
MODELS_DIR="$LIBMELLO_DIR/models"
if [ ! -f "$MODELS_DIR/silero_vad.onnx" ]; then
    echo ""
    echo "--- Downloading Silero VAD model ---"
    mkdir -p "$MODELS_DIR"
    curl -L "https://github.com/snakers4/silero-vad/raw/v5.1/src/silero_vad/data/silero_vad.onnx" \
        -o "$MODELS_DIR/silero_vad.onnx"
    echo "Downloaded to: $MODELS_DIR/silero_vad.onnx"
else
    echo "Silero VAD model already present, skipping"
fi

echo ""
echo "=== Setup complete ==="
echo ""
echo "vcpkg deps (opus, libdatachannel) will be installed automatically"
echo "during the first 'cargo build' via cmake manifest mode."
echo ""
echo "To build:"
echo "  cd $ROOT_DIR"
echo "  cargo build"
