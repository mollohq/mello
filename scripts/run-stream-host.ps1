#!/usr/bin/env pwsh
# Stream host probe — local dev by default (no env vars required).
#
#   .\scripts\run-stream-host.ps1
#   .\scripts\run-stream-viewer.ps1
#
# Remote / prod:
#   $env:NAKAMA_SERVER_KEY = "..."
#   $env:MELLO_CREW_ID = "..."
#   .\scripts\run-stream-host.ps1 -Remote
#   .\scripts\run-stream-viewer.ps1 -Remote

param(
    [switch]$Remote,
    [string]$NakamaHttpBase,
    [string]$ServerKey,
    [string]$NakamaAuthToken,
    [string]$CrewId,
    [string]$CrewName,
    [switch]$RefreshAuth,
    [int]$Fps = 60,
    [int]$BitrateKbps = 4000,
    [int]$RequestWidth = 1280,
    [int]$RequestHeight = 720,
    [string]$SourceTitleSubstring = "",
    [int]$SourceIndex = 0,
    [string]$StreamTitle = "Stream Host Probe",
    [switch]$SupportsAv1,
    [string]$HostLog = "C:\temp\host.log"
)

$ErrorActionPreference = "Stop"

. (Join-Path $PSScriptRoot "stream-probe-common.ps1")

$ctx = Initialize-StreamProbeContext `
    -Remote:$Remote `
    -NakamaHttpBase $NakamaHttpBase `
    -ServerKey $ServerKey `
    -AuthToken $NakamaAuthToken `
    -CrewId $CrewId `
    -CrewName $CrewName `
    -RefreshAuth:$RefreshAuth

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
        "--nakama-http-base", $ctx.NakamaHttpBase,
        "--nakama-auth-token", $ctx.AuthToken,
        "--crew-id", $ctx.CrewId,
        "--stream-title", $StreamTitle,
        "--request-width", $RequestWidth,
        "--request-height", $RequestHeight
    )

    if ($SupportsAv1) {
        $cargoArgs += "--supports-av1"
    }
    if ($SourceIndex -gt 0) {
        $cargoArgs += @("--source-index", $SourceIndex)
    }
    elseif (-not [string]::IsNullOrWhiteSpace($SourceTitleSubstring)) {
        $cargoArgs += @("--source-title-substring", $SourceTitleSubstring)
    }

    Write-Host ""
    Write-Host "Starting stream host probe..." -ForegroundColor Cyan
    Write-Host "Profile:  $($ctx.ProfileName)" -ForegroundColor DarkGray
    Write-Host "Crew:     $($ctx.CrewId)" -ForegroundColor DarkGray
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
