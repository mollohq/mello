#!/usr/bin/env bash
# Test the Velopack auto-updater locally on macOS without Developer ID signing.
#
# Usage:
#   ./scripts/test-updater.sh --action scenario          # pack/install v0.1.0, pack v0.2.0, launch
#   ./scripts/test-updater.sh                            # pack v0.1.0 (default)
#   ./scripts/test-updater.sh --version 0.2.0            # pack v0.2.0
#   ./scripts/test-updater.sh --action install           # install matching --version from vpk-out
#   ./scripts/test-updater.sh --action launch            # launch installed app with local update source
#   ./scripts/test-updater.sh --action clean             # delete local dist/vpk-out test artifacts

set -euo pipefail

ACTION="pack"
VERSION="0.1.0"
OLD_VERSION="0.1.0"
NEW_VERSION="0.2.0"
APPLY_DELAY_SECONDS=10
NO_RESTORE_VERSION=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"
VPK_OUT="$REPO_ROOT/vpk-out"
CHANNEL="osx-arm64-dev"
PACK_ID="m3llo"
CARGO_TOML="$REPO_ROOT/Cargo.toml"
CARGO_LOCK="$REPO_ROOT/Cargo.lock"
INFO_PLIST="$REPO_ROOT/client/macos/Info.plist"
ICON_PATH="$REPO_ROOT/client/assets/icons/mello.icns"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
case "$CARGO_TARGET_DIR" in
    /*) ;;
    *) CARGO_TARGET_DIR="$REPO_ROOT/$CARGO_TARGET_DIR" ;;
esac

usage() {
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
    echo "  [!] $*" >&2
    exit 1
}

log_header() {
    echo ""
    echo "  mello updater test (macOS)"
    echo "  --------------------------"
    echo ""
}

assert_macos() {
    [[ "$(uname -s)" == "Darwin" ]] || die "This script is macOS-only."
}

assert_vpk() {
    if ! command -v vpk >/dev/null 2>&1; then
        die "vpk not found. Install it with: dotnet tool install -g vpk"
    fi
}

assert_pack_prereqs() {
    [[ -f "$INFO_PLIST" ]] || die "Info.plist not found at $INFO_PLIST"
    [[ -f "$ICON_PATH" ]] || die "mello.icns not found at $ICON_PATH"
}

get_workspace_version() {
    python3 - "$CARGO_TOML" <<'PY'
import re
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
if not match:
    raise SystemExit(f"Could not read workspace package version from {path}")
print(match.group(1))
PY
}

set_workspace_version() {
    local target_version="$1"
    python3 - "$CARGO_TOML" "$target_version" <<'PY'
import re
import sys

path, version = sys.argv[1], sys.argv[2]
text = open(path, encoding="utf-8").read()
updated, count = re.subn(
    r'(?m)^(version\s*=\s*")[^"]+(")',
    rf'\g<1>{version}\2',
    text,
    count=1,
)
if count != 1:
    raise SystemExit(f"Could not update workspace package version in {path}")
open(path, "w", encoding="utf-8").write(updated)
PY
}

set_plist_version() {
    local target_version="$1"

    /usr/libexec/PlistBuddy -c "Add :CFBundleShortVersionString string $target_version" "$INFO_PLIST" 2>/dev/null \
        || /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $target_version" "$INFO_PLIST"
    /usr/libexec/PlistBuddy -c "Add :CFBundleVersion string $target_version" "$INFO_PLIST" 2>/dev/null \
        || /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $target_version" "$INFO_PLIST"
}

invoke_with_workspace_version() {
    local build_version="$1"
    shift

    local original_version
    original_version="$(get_workspace_version)"

    local plist_backup
    plist_backup="$(mktemp)"
    cp "$INFO_PLIST" "$plist_backup"

    local lock_backup
    lock_backup="$(mktemp)"
    cp "$CARGO_LOCK" "$lock_backup"

    set_workspace_version "$build_version"
    set_plist_version "$build_version"

    local status=0
    set +e
    (
        set -euo pipefail
        "$@"
    )
    status=$?
    set -e

    if [[ "$NO_RESTORE_VERSION" -eq 0 ]]; then
        set_workspace_version "$original_version"
        cp "$plist_backup" "$INFO_PLIST"
        cp "$lock_backup" "$CARGO_LOCK"
    fi
    rm -f "$plist_backup" "$lock_backup"

    return "$status"
}

copy_onnxruntime_dylibs() {
    shopt -s nullglob

    local dylibs=("$REPO_ROOT"/libmello/third_party/onnxruntime/onnxruntime-osx-arm64-*/lib/libonnxruntime*.dylib)
    if [[ "${#dylibs[@]}" -eq 0 ]]; then
        dylibs=("$CARGO_TARGET_DIR"/release/libonnxruntime*.dylib)
    fi

    [[ "${#dylibs[@]}" -gt 0 ]] || die "No libonnxruntime*.dylib found. Run ./scripts/setup-macos.sh first."
    cp -P "${dylibs[@]}" "$DIST_DIR/"
}

copy_silero_model() {
    local model
    model="$(find "$CARGO_TARGET_DIR/release/build" -name silero_vad.onnx -print -quit 2>/dev/null || true)"

    if [[ -n "$model" ]]; then
        cp "$model" "$DIST_DIR/"
    else
        echo "    [warn] silero_vad.onnx not found in build output"
    fi
}

do_clean() {
    echo "  Cleaning local updater artifacts..."
    rm -rf "$DIST_DIR" "$VPK_OUT"
    echo "  done."
    echo ""
}

do_pack_body() {
    local pack_version="$1"

    echo "  [1/3] Building client (release) v$pack_version..."
    (cd "$REPO_ROOT" && cargo build --release -p mello-client)

    echo "  [2/3] Assembling dist..."
    rm -rf "$DIST_DIR"
    mkdir -p "$DIST_DIR"
    cp "$CARGO_TARGET_DIR/release/mello" "$DIST_DIR/"
    copy_onnxruntime_dylibs
    copy_silero_model

    echo "  [3/3] Packing v$pack_version (channel: $CHANNEL)..."
    mkdir -p "$VPK_OUT"
    vpk pack \
        --packId "$PACK_ID" \
        --packVersion "$pack_version" \
        --packDir "$DIST_DIR" \
        --mainExe mello \
        --packTitle Mello \
        --channel "$CHANNEL" \
        --outputDir "$VPK_OUT" \
        --icon "$ICON_PATH" \
        --plist "$INFO_PLIST"
}

do_pack() {
    local pack_version="$1"
    assert_vpk
    assert_pack_prereqs

    invoke_with_workspace_version "$pack_version" do_pack_body "$pack_version"

    echo ""
    echo "  done! Packed v$pack_version -> vpk-out/"
    if [[ "$NO_RESTORE_VERSION" -eq 0 ]]; then
        echo "  restored workspace version to $(get_workspace_version)"
    fi
    echo ""
    echo "  Next steps:"
    echo "    Install:  ./scripts/test-updater.sh --action install --version $pack_version"
    echo "    Launch:   ./scripts/test-updater.sh --action launch"
    echo ""
}

select_pkg() {
    local install_version="$1"
    shopt -s nullglob

    local candidates=("$VPK_OUT"/*"$install_version"*"$CHANNEL"*Setup*.pkg)
    if [[ "${#candidates[@]}" -eq 0 ]]; then
        candidates=("$VPK_OUT"/*"$CHANNEL"*Setup*.pkg)
    fi
    if [[ "${#candidates[@]}" -eq 0 ]]; then
        candidates=("$VPK_OUT"/*Setup*.pkg)
    fi

    [[ "${#candidates[@]}" -gt 0 ]] || die "No Setup.pkg found in vpk-out/. Run pack first."
    printf '%s\n' "${candidates[@]}" | sort -r | head -n 1
}

do_install() {
    local install_version="$1"
    local setup
    setup="$(select_pkg "$install_version")"

    echo "  Installing $(basename "$setup")..."
    installer -pkg "$setup" -target CurrentUserHomeDirectory

    echo ""
    echo "  done! Mello installed."
    echo ""
    echo "  Launch with:  ./scripts/test-updater.sh --action launch"
    echo ""
}

find_installed_app_binary() {
    local app_paths=(
        "$HOME/Applications/Mello.app"
        "$HOME/Applications/m3llo.app"
        "/Applications/Mello.app"
        "/Applications/m3llo.app"
    )

    local app
    for app in "${app_paths[@]}"; do
        if [[ -x "$app/Contents/MacOS/mello" ]]; then
            printf '%s\n' "$app/Contents/MacOS/mello"
            return 0
        fi
    done

    return 1
}

do_launch() {
    local exe
    exe="$(find_installed_app_binary)" || die "Mello.app not found. Run: ./scripts/test-updater.sh --action install"

    echo "  Launching installed Mello..."
    echo "  App binary       = $exe"
    echo "  MELLO_UPDATE_URL = $VPK_OUT"
    echo "  MELLO_UPDATE_CHANNEL = $CHANNEL"
    echo "  RUST_LOG         = debug"
    echo "  Apply delay      = ${APPLY_DELAY_SECONDS}s"
    echo "  Expectation      = force-update window if installed version is older than vpk-out latest"
    echo ""

    MELLO_UPDATE_URL="$VPK_OUT" \
        MELLO_UPDATE_CHANNEL="$CHANNEL" \
        RUST_LOG="debug" \
        MELLO_UPDATE_APPLY_DELAY_MS="$((APPLY_DELAY_SECONDS * 1000))" \
        "$exe"
}

do_scenario() {
    echo "  Force-update scenario: v$OLD_VERSION -> v$NEW_VERSION"
    echo ""
    do_clean
    do_pack "$OLD_VERSION"
    do_install "$OLD_VERSION"
    do_pack "$NEW_VERSION"
    do_launch
}

parse_args() {
    while [[ "$#" -gt 0 ]]; do
        case "$1" in
            -a|--action|-Action)
                ACTION="${2:-}"
                shift 2
                ;;
            -v|--version|-Version)
                VERSION="${2:-}"
                shift 2
                ;;
            --old-version|-OldVersion)
                OLD_VERSION="${2:-}"
                shift 2
                ;;
            --new-version|-NewVersion)
                NEW_VERSION="${2:-}"
                shift 2
                ;;
            --apply-delay-seconds|-ApplyDelaySeconds)
                APPLY_DELAY_SECONDS="${2:-}"
                shift 2
                ;;
            --no-restore-version|-NoRestoreVersion)
                NO_RESTORE_VERSION=1
                shift
                ;;
            -h|--help|-Help)
                usage
                exit 0
                ;;
            *)
                die "Unknown argument: $1"
                ;;
        esac
    done

    case "$ACTION" in
        pack|install|launch|scenario|clean) ;;
        *) die "Invalid action '$ACTION'. Expected pack, install, launch, scenario, or clean." ;;
    esac

    [[ "$APPLY_DELAY_SECONDS" =~ ^[0-9]+$ ]] || die "--apply-delay-seconds must be an integer."
}

parse_args "$@"
assert_macos
log_header

case "$ACTION" in
    pack) do_pack "$VERSION" ;;
    install) do_install "$VERSION" ;;
    launch) do_launch ;;
    scenario) do_scenario ;;
    clean) do_clean ;;
esac
