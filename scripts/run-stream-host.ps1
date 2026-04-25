#!/usr/bin/env pwsh

param(
    [string]$NakamaHttpBase = $env:MELLO_NAKAMA_HTTP_BASE,
    [string]$NakamaAuthToken = $env:MELLO_NAKAMA_AUTH_TOKEN,
    [string]$CrewId = $env:MELLO_CREW_ID,
    [int]$Fps = 30,
    [int]$BitrateKbps = 5000,
    [int]$RequestWidth = 1920,
    [int]$RequestHeight = 1080,
    [string]$StreamTitle = "Stream Host Probe",
    [switch]$SupportsAv1,
    [string]$HostLog = "C:\temp\host.log"
)

$ErrorActionPreference = "Stop"

function Assert-Required([string]$Name, [string]$Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) {
        throw "Missing required value for $Name. Provide -$Name or set matching env var."
    }
}

Assert-Required "NakamaHttpBase" $NakamaHttpBase
Assert-Required "NakamaAuthToken" $NakamaAuthToken
Assert-Required "CrewId" $CrewId

$logDir = Split-Path -Parent $HostLog
if (-not [string]::IsNullOrWhiteSpace($logDir) -and -not (Test-Path $logDir)) {
    New-Item -ItemType Directory -Path $logDir -Force | Out-Null
}

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    $cargoArgs = @(
        "run", "--release", "-p", "stream-host", "--",
        "--fps", $Fps,
        "--bitrate", $BitrateKbps,
        "--nakama-start-stream",
        "--nakama-http-base", $NakamaHttpBase,
        "--nakama-auth-token", $NakamaAuthToken,
        "--crew-id", $CrewId,
        "--stream-title", $StreamTitle,
        "--request-width", $RequestWidth,
        "--request-height", $RequestHeight
    )

    if ($SupportsAv1) {
        $cargoArgs += "--supports-av1"
    }

    Write-Host ""
    Write-Host "Starting stream host probe..." -ForegroundColor Cyan
    Write-Host "Host log: $HostLog" -ForegroundColor DarkGray
    Write-Host ""

    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $hadNativePref = Test-Path Variable:PSNativeCommandUseErrorActionPreference
    try {
        if ($hadNativePref) {
            $prevNativePref = $PSNativeCommandUseErrorActionPreference
            $PSNativeCommandUseErrorActionPreference = $false
        }
        & cargo @cargoArgs 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $HostLog
        $cmdExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $prevEap
        if ($hadNativePref) {
            $PSNativeCommandUseErrorActionPreference = $prevNativePref
        }
    }
    if ($cmdExitCode -ne 0) {
        throw "stream-host failed (exit code $cmdExitCode)"
    }
    exit $cmdExitCode
}
finally {
    Pop-Location
}
