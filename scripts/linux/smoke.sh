#!/usr/bin/env bash
# Smoke test of the Linux AppImage without a display. Runs in the build
# container: scripts/linux-build.sh exec scripts/linux/smoke.sh [SECONDS]
#
# Unpacks the AppImage (Rosetta on a Mac cannot start an AppImage, and the
# container has no FUSE), runs the app under Xvfb for SECONDS (default 20),
# and fails if the app exits before that or logs a panic.
set -euo pipefail

SECONDS_TO_RUN=${1:-20}
APPIMAGE=${APPIMAGE_PATH:-/dist/m3llo-linux-x64-stable.AppImage}
WORK=/tmp/m3llo-smoke
rm -rf "$WORK" "$HOME/.local/share/mello/logs"
mkdir -p "$WORK"

# The squashfs image starts where the ELF runtime ends: section header
# offset plus the size of the section header table.
offset=$(python3 -c "
import struct, sys
h = open(sys.argv[1], 'rb').read(64)
print(struct.unpack_from('<Q', h, 0x28)[0] + struct.unpack_from('<H', h, 0x3A)[0] * struct.unpack_from('<H', h, 0x3C)[0])
" "$APPIMAGE")
unsquashfs -q -o "$offset" -d "$WORK/root" "$APPIMAGE" >/dev/null

set +e
xvfb-run -a -s "-screen 0 1920x1080x24" timeout "$SECONDS_TO_RUN" "$WORK/root/AppRun" >"$WORK/run.out" 2>&1
code=$?
set -e

log=$(ls "$HOME"/.local/share/mello/logs/mello.log* 2>/dev/null | head -1 || true)
echo "--- app output (last 30 lines)"
tail -30 "$WORK/run.out"

fail=0
if [ "$code" -ne 124 ]; then
    echo "smoke: the app exited with code $code before ${SECONDS_TO_RUN}s" >&2
    fail=1
fi
if grep -q "PANIC" "$WORK/run.out" ${log:+"$log"}; then
    echo "smoke: the app logged a panic" >&2
    grep -h "PANIC" -A2 "$WORK/run.out" ${log:+"$log"} | head -10 >&2
    fail=1
fi
[ "$fail" -eq 0 ] && echo "smoke: ok, the app ran for ${SECONDS_TO_RUN}s without a panic"
exit "$fail"
