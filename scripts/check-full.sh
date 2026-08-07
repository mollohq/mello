#!/bin/sh
# Full gate: everything scripts/check.sh runs, plus the two suites that need a
# native toolchain — libmello's C++ tests and the Nakama Go modules.
#
# Slower than check.sh (the first libmello configure resolves vcpkg packages
# into a fresh build dir and can take 10+ minutes cold; later runs reuse the
# vcpkg binary cache). Run before a release, or when touching libmello/ or
# backend/.
#
# Usage:  ./scripts/check-full.sh
set -eu

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

export CI=true

step() {
    printf '\n\033[1m▸ %s\033[0m\n' "$1"
}

FAILED=0
run() {
    if ! "$@"; then
        FAILED=1
        printf '\033[31m  ✗ failed: %s\033[0m\n' "$*"
    fi
}

START=$(date +%s)

step "rust: fmt / clippy / tests"
run cargo fmt --all -- --check
run cargo clippy --all-targets -- -D warnings
run cargo test --workspace

# MELLO_BUILD_TESTS defaults to OFF: mello-sys/build.rs configures libmello on
# every cargo build and we do not want the gtest suite compiled into every dev
# build. CMAKE_TOOLCHAIN_FILE must be absolute — CMake resolves a relative
# value against the build directory, not the shell's cwd.
step "c++: libmello ctest"
case "$(uname -s)" in
    Darwin)
        ARCH_ARGS="-DVCPKG_TARGET_TRIPLET=arm64-osx -DVCPKG_HOST_TRIPLET=arm64-osx -DCMAKE_OSX_ARCHITECTURES=arm64 -DCMAKE_OSX_DEPLOYMENT_TARGET=15.0"
        ;;
    *)
        ARCH_ARGS="-DVCPKG_TARGET_TRIPLET=x64-windows-static-md"
        ;;
esac

# shellcheck disable=SC2086
run cmake -B libmello/build-ci -S libmello \
    -DMELLO_BUILD_TESTS=ON \
    -DCMAKE_TOOLCHAIN_FILE="$ROOT/external/vcpkg/scripts/buildsystems/vcpkg.cmake" \
    -DCMAKE_BUILD_TYPE=Release \
    $ARCH_ARGS
run cmake --build libmello/build-ci
# VideoPipelineTest does real monitor capture; its SetUp skips when CI is set.
# Without that it dies with SIGTRAP on a machine lacking screen-recording
# permission — a failure that has nothing to do with your change.
case "$(uname -s)" in
    Darwin)
        # See .github/workflows/pr-checks.yml for why these two are skipped on
        # macOS. Windows runs the full suite.
        run ctest --test-dir libmello/build-ci --output-on-failure \
            -E "QueueOverflowEntersIdrGate|BoundedBurstRequestsLocalIdrOnQueueOverflow"
        ;;
    *)
        run ctest --test-dir libmello/build-ci --output-on-failure
        ;;
esac

step "go: nakama modules"
if command -v go >/dev/null 2>&1; then
    (
        cd backend/nakama/data/modules
        unformatted=$(gofmt -l .)
        if [ -n "$unformatted" ]; then
            printf '\033[31m  ✗ gofmt: %s\033[0m\n' "$unformatted"
            exit 1
        fi
        go vet ./...
        go test ./...
    ) || FAILED=1
else
    printf '  ! go not installed, skipping backend tests\n'
fi

ELAPSED=$(( $(date +%s) - START ))

printf '\n'
if [ "$FAILED" -eq 0 ]; then
    printf '\033[32m━━━ all checks passed in %ss ━━━\033[0m\n' "$ELAPSED"
else
    printf '\033[31m━━━ checks FAILED after %ss ━━━\033[0m\n' "$ELAPSED"
    exit 1
fi
