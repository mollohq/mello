#!/usr/bin/env bash
# Build a Linux AppImage for testing, in the Ubuntu 22.04 build container.
#
# Usage: scripts/linux-package.sh [VERSION] [CHANNEL]
#   VERSION defaults to 0.0.<yyDDDHHMM> in UTC (year, day of year, time), so
#   each build sorts after the one before it and a local Velopack feed sees it
#   as an update. vpk needs a version of at least 0.0.1, and NuGet keeps each
#   part under 2^31, so a full date-time does not fit in one part.
# Output: dist-linux/ (the AppImage, the full package and the release files).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=${1:-0.0.$(date -u +%y%j%H%M)}
CHANNEL=${2:-linux-x64-stable}
mkdir -p dist-linux
exec scripts/linux-build.sh exec env FEATURES="${FEATURES:-}" scripts/linux/package.sh "$VERSION" "$CHANNEL"
