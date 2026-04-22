#!/usr/bin/env pwsh

param(
    [string]$NakamaHttpBase = $env:MELLO_NAKAMA_HTTP_BASE,
    [string]$NakamaAuthToken = $env:MELLO_NAKAMA_AUTH_TOKEN,
    [string]$Session = "auto",
    [string]$HostLog = "C:\temp\host.log",
    [string]$ViewerLog = "C:\temp\viewer.log",
    [string]$Role = "viewer",
    [int]$Width = 0,
    [int]$Height = 0,
    [bool]$NativeMetrics = $true
)

$ErrorActionPreference = "Stop"

function Assert-Required([string]$Name, [string]$Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) {
        throw "Missing required value for $Name. Provide -$Name or set matching env var."
    }
}

function Get-LatestSessionFromHostLog([string]$Path) {
    if (-not (Test-Path $Path)) {
        return $null
    }

    $pattern = "Nakama start_stream:\s+mode=sfu\s+session=([^\s]+)"
    $matches = Select-String -Path $Path -Pattern $pattern
    if (-not $matches -or $matches.Count -eq 0) {
        return $null
    }
    return $matches[-1].Matches[0].Groups[1].Value
}

Assert-Required "NakamaHttpBase" $NakamaHttpBase
Assert-Required "NakamaAuthToken" $NakamaAuthToken

if ($Session -eq "auto") {
    $detectedSession = Get-LatestSessionFromHostLog -Path $HostLog
    if ([string]::IsNullOrWhiteSpace($detectedSession)) {
        throw "Session auto-detect failed. Run host first, or pass -Session <session_id>."
    }
    $Session = $detectedSession
}

$logDir = Split-Path -Parent $ViewerLog
if (-not [string]::IsNullOrWhiteSpace($logDir) -and -not (Test-Path $logDir)) {
    New-Item -ItemType Directory -Path $logDir -Force | Out-Null
}

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    $cargoArgs = @(
        "run", "--release", "-p", "sfu-stream-viewer-probe", "--",
        "--watch-stream-print",
        "--nakama-http-base", $NakamaHttpBase,
        "--nakama-auth-token", $NakamaAuthToken,
        "--session", $Session,
        "--role", $Role
    )

    if ($Width -gt 0) {
        $cargoArgs += @("--width", $Width)
    }
    if ($Height -gt 0) {
        $cargoArgs += @("--height", $Height)
    }
    if ($NativeMetrics) {
        $cargoArgs += "--native-metrics"
    }

    Write-Host ""
    Write-Host "Starting stream viewer probe..." -ForegroundColor Cyan
    Write-Host "Session:   $Session" -ForegroundColor DarkGray
    Write-Host "Viewer log: $ViewerLog" -ForegroundColor DarkGray
    Write-Host ""

    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $hadNativePref = Test-Path Variable:PSNativeCommandUseErrorActionPreference
    try {
        if ($hadNativePref) {
            $prevNativePref = $PSNativeCommandUseErrorActionPreference
            $PSNativeCommandUseErrorActionPreference = $false
        }
        & cargo @cargoArgs 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $ViewerLog
        $cmdExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $prevEap
        if ($hadNativePref) {
            $PSNativeCommandUseErrorActionPreference = $prevNativePref
        }
    }
    if ($cmdExitCode -ne 0) {
        throw "sfu-stream-viewer-probe failed (exit code $cmdExitCode)"
    }
    exit $cmdExitCode
}
finally {
    Pop-Location
}
