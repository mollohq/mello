# Resets and cleans the workspace of a self-hosted runner, and keeps what the
# libmello CMake build downloads into libmello/ while it is still current.
# release.yml and pr-checks.yml (Windows) run it right after a checkout with
# clean: false, before any build. clean-workspace.sh is the same for macOS.
#
# - libmello/third_party/onnxruntime: only the folder of the ORT_VERSION in
#   libmello/CMakeLists.txt stays, so an older ORT is never linked or packed.
# - libmello/models: stays only while the model lines of
#   libmello/CMakeLists.txt hash to the stamp that the last run wrote. CMake
#   downloads a model only when the file is missing, so a changed model URL
#   needs a fresh download.
#
# Everything else that git does not track is deleted.

Set-Location (Join-Path $PSScriptRoot '../..')

git reset --hard HEAD
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$cmake = Get-Content libmello/CMakeLists.txt
# No ORT_VERSION line: keep no ORT (the pattern matches nothing).
$m = $cmake | Select-String '^set\(ORT_VERSION "([^"]+)"\)'
$ort = if ($m) { $m.Matches[0].Groups[1].Value } else { 'none' }
$keep = @('-e', "libmello/third_party/onnxruntime/onnxruntime-*-$ort/")

$sources = ($cmake | Select-String 'silero|ECHO_SUPPRESSOR' | ForEach-Object Line) -join "`n"
$stamp = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($sources)))
$stampFile = 'libmello/models/.sources-sha256'
if ((Test-Path $stampFile) -and (Get-Content $stampFile -Raw).Trim() -eq $stamp) {
    $keep += @('-e', 'libmello/models/')
} else {
    Write-Host "The model sources in libmello/CMakeLists.txt changed (or no stamp): the models are downloaded again."
}

git clean -ffdx @keep
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force libmello/models | Out-Null
Set-Content -Path $stampFile -Value $stamp
exit 0
