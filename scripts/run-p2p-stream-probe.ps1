#!/usr/bin/env pwsh
# Native RTP P2P validation using libmello mello_rtp_tests (same AU APIs as product code).
#
#   .\scripts\run-p2p-stream-probe.ps1

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$libmelloBuild = Join-Path $repoRoot "libmello\build"
$testExe = Join-Path $libmelloBuild "tests\Release\mello_rtp_tests.exe"

Push-Location $repoRoot
try {
    cmake --build $libmelloBuild --config Release --target mello_rtp_tests
    if ($LASTEXITCODE -ne 0) { throw "mello_rtp_tests build failed" }
    if (-not (Test-Path $testExe)) { throw "missing $testExe" }
    & $testExe
    if ($LASTEXITCODE -ne 0) { throw "mello_rtp_tests failed (exit $LASTEXITCODE)" }
    Write-Host "P2P native RTP probe passed" -ForegroundColor Green
}
finally {
    Pop-Location
}
