#!/usr/bin/env bash
# Resets and cleans the workspace of a self-hosted runner, and keeps what the
# libmello CMake build downloads into libmello/ while it is still current.
# release.yml (macOS) runs it right after a checkout with clean: false,
# before any build. clean-workspace.ps1 is the same for Windows.
#
# - libmello/third_party/onnxruntime: only the folder of the ORT_VERSION in
#   libmello/CMakeLists.txt stays, so an older ORT is never linked or packed.
# - libmello/models: stays only while the model lines of
#   libmello/CMakeLists.txt hash to the stamp that the last run wrote. CMake
#   downloads a model only when the file is missing, so a changed model URL
#   needs a fresh download.
#
# Everything else that git does not track is deleted.

set -euo pipefail

# All in one function: bash reads a script while it runs it, and
# `git reset --hard` can write this file.
main() {
    cd "$(dirname "$0")/../.."

    git reset --hard HEAD

    local ort
    ort=$(sed -n 's/^set(ORT_VERSION "\([^"]*\)")$/\1/p' libmello/CMakeLists.txt)
    # No ORT_VERSION line: keep no ORT (the pattern matches nothing).
    ort=${ort:-none}
    local keep=(-e "libmello/third_party/onnxruntime/onnxruntime-*-$ort/")

    local stamp stamp_file=libmello/models/.sources-sha256
    stamp=$(grep -iE 'silero|ECHO_SUPPRESSOR' libmello/CMakeLists.txt | shasum -a 256 | cut -d' ' -f1)
    if [ -f "$stamp_file" ] && [ "$(cat "$stamp_file")" = "$stamp" ]; then
        keep+=(-e libmello/models/)
    else
        echo "The model sources in libmello/CMakeLists.txt changed (or no stamp): the models are downloaded again."
    fi

    git clean -ffdx "${keep[@]}"

    mkdir -p libmello/models
    echo "$stamp" > "$stamp_file"
}

main "$@"; exit $?
