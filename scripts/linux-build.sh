#!/usr/bin/env bash
# Run cargo for Linux x86_64 in the Ubuntu 22.04 build container.
#
# The container builds from its own copy of the source, in a Docker volume.
# A bind mount of this checkout would put a Linux vcpkg binary and Linux
# build trees into external/vcpkg and libmello/, where the macOS build of the
# same checkout would then find them.
#
# The copy sits at the same absolute path as this checkout, and the repo's git
# directory is mounted read-only at its own path. The worktree's .git file and
# the submodules' relative gitdir links then resolve, so build-stamp records
# the real commit and dirty state.
#
# Usage:
#   scripts/linux-build.sh                  cargo build --release (production client)
#   scripts/linux-build.sh check --workspace
#   scripts/linux-build.sh shell            an interactive shell in the container
#   scripts/linux-build.sh exec CMD ARGS    run CMD in /src in the container
#
# `cargo fetch` runs first: client/build_notices.rs calls `cargo tree --offline`
# for every platform, so the crates of every target must be in the cache.
#
# Memory: one rustc on the Slint-generated client code needs more than the
# 8 GB of a default Docker Desktop VM under Rosetta. Give Docker 24 GB
# (Settings, Resources). Six parallel jobs by default; CARGO_BUILD_JOBS
# overrides it.
#
# Build output stays in the m3llo-linux-target volume. /dist in the container
# is dist-linux/ in this checkout, for files that must leave the container.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=m3llo-linux-build:22.04
docker build --quiet --platform linux/amd64 -t "$IMAGE" -f scripts/linux/Dockerfile scripts/linux >/dev/null

if [ $# -eq 0 ]; then
    set -- build --release --locked --features mello-client/production -p mello-client
fi

# -i always, so `shell` also reads commands from a pipe. -t only on a terminal.
TTY=(-i)
[ -t 0 ] && TTY=(-it)

# Exclusions: build output, vcpkg's per-host binary and trees, and the files
# that libmello's CMake downloads into the source tree at configure time
# (ONNX Runtime, the RNNoise model, the ONNX models). Without the exclusions
# --delete removes those downloads and the next build fails.
SRC="$PWD"
GIT_COMMON=$(git rev-parse --path-format=absolute --git-common-dir)
GIT_LINK=""
[ -f .git ] && GIT_LINK=$(cat .git)

SYNC='rsync -a --delete \
    --exclude=/.git --exclude=/target/ --exclude=/dist-linux/ \
    --exclude=/external/vcpkg/vcpkg --exclude=/external/vcpkg/buildtrees/ \
    --exclude=/external/vcpkg/packages/ --exclude=/external/vcpkg/downloads/ \
    --exclude=/external/vcpkg/installed/ \
    --exclude=/libmello/build/ --exclude=/libmello/build-ci/ \
    --exclude=/libmello/third_party/onnxruntime/ \
    --exclude=/libmello/third_party/rnnoise/src/rnnoise_data.c \
    --exclude=/libmello/third_party/rnnoise/src/rnnoise_data.h \
    --exclude=/libmello/models/ \
    --exclude=/external/vcpkg-cache/ \
    --exclude=.DS_Store \
    /host/ "$SRC"/'

exec docker run --rm ${TTY[@]+"${TTY[@]}"} --platform linux/amd64 \
    -v "$PWD":/host:ro \
    -v "$PWD/dist-linux":/dist \
    -v "$GIT_COMMON":"$GIT_COMMON":ro \
    -v m3llo-linux-src:"$SRC" \
    -v m3llo-linux-target:"$SRC/target" \
    -v m3llo-linux-vcpkg:"$SRC/external/vcpkg-cache" \
    -v m3llo-linux-cargo:/usr/local/cargo/registry \
    -w "$SRC" \
    -e CI=true \
    -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}" \
    -e VCPKG_DEFAULT_BINARY_CACHE="$SRC/external/vcpkg-cache" \
    -e SRC="$SRC" \
    -e M3LLO_GIT_LINK="$GIT_LINK" \
    -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0='*' \
    "$IMAGE" bash -c "set -e; $SYNC && if [ -n \"\$M3LLO_GIT_LINK\" ]; then printf '%s\n' \"\$M3LLO_GIT_LINK\" > .git; fi && cargo fetch --locked --quiet && case \"\$1\" in shell) exec bash ;; exec) shift; exec \"\$@\" ;; *) exec cargo \"\$@\" ;; esac" bash "$@"
