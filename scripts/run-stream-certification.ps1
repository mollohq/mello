#!/usr/bin/env pwsh
# RTP stream certification gates (Slice 9). Requires local Nakama + SFU.
#
#   .\scripts\run-stream-certification.ps1
#   .\scripts\run-stream-certification.ps1 -SkipLan -SkipLoss

param(
    [switch]$SkipSoak,
    [switch]$SkipLan,
    [switch]$SkipLoss,
    [switch]$SkipProductClient,
    [int]$LanHoldSec = 65,
    [string]$CaptureSourceTitle = "Heaven",
    [string]$CertDir = "C:\temp\mello-stream-cert"
)

$ErrorActionPreference = "Stop"

. (Join-Path $PSScriptRoot "stream-probe-common.ps1")

$repoRoot = Split-Path -Parent $PSScriptRoot
$sfuRoot = Join-Path (Split-Path -Parent $repoRoot) "mello-sfu"
$env:CARGO_TARGET_DIR = Join-Path $repoRoot "target"
$probeCtx = Initialize-StreamProbeContext

New-Item -ItemType Directory -Path $CertDir -Force | Out-Null

$results = [System.Collections.Generic.List[object]]::new()

function Add-GateResult(
    [string]$Name,
    [bool]$Passed,
    [string]$Detail
) {
    $script:results.Add([pscustomobject]@{
            Gate   = $Name
            Passed = $Passed
            Detail = $Detail
        })
    $color = if ($Passed) { "Green" } else { "Red" }
    $status = if ($Passed) { "PASS" } else { "FAIL" }
    Write-Host "[$status] $Name — $Detail" -ForegroundColor $color
}

function Test-Prerequisites {
    if (-not (Test-NakamaReachable $probeCtx.NakamaHttpBase)) {
        throw "Nakama not reachable at $($probeCtx.NakamaHttpBase). Run backend docker compose."
    }
    $profile = Get-StreamProbeProfile $false $probeCtx.NakamaHttpBase $null
    if (-not (Test-LocalSfuReachable $profile.SfuHealthUrl)) {
        throw "SFU not reachable at $($profile.SfuHealthUrl). Start mello-sfu locally."
    }
    Add-GateResult "prerequisites" $true "Nakama + SFU reachable"
}

function Invoke-StreamSoakGate(
    [string]$Name,
    [int]$Viewers,
    [int]$HoldMs,
    [double]$MinRxRatio,
    [double]$MaxP95Ms,
    [int]$MaxQueueDrops = 0
) {
    $log = Join-Path $CertDir "soak-${Viewers}v-${HoldMs}ms.log"
    $adminPassword = $env:SFU_ADMIN_PASSWORD
    if ($null -eq $adminPassword) { $adminPassword = "" }
    Push-Location $sfuRoot
    try {
        go run ./tools/stream-soak `
            --transport rtp `
            --endpoint ws://127.0.0.1:8443/ws `
            --admin-password $adminPassword `
            --viewers $Viewers `
            --hold-ms $HoldMs `
            --fps 60 `
            --packets-per-frame 3 `
            --keyframe-interval 60 `
            --min-rx-ratio $MinRxRatio `
            --max-p95-ms $MaxP95Ms `
            --max-queue-drops $MaxQueueDrops `
            2>&1 | Tee-Object -FilePath $log
        $ok = ($LASTEXITCODE -eq 0)
    }
    finally {
        Pop-Location
    }
    Add-GateResult $Name $ok "viewers=$Viewers hold_ms=$HoldMs (log: $log)"
}

function Wait-ForHostSession([string]$HostLog, [int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $pattern = "Nakama start_stream:\s+mode=sfu\s+session=([^\s]+)"
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $HostLog) {
            $m = Select-String -Path $HostLog -Pattern $pattern -ErrorAction SilentlyContinue
            if ($m) {
                return $m[-1].Matches[0].Groups[1].Value
            }
        }
        Start-Sleep -Seconds 2
    }
    return $null
}

function Test-ViewerCertLog(
    [string]$ViewerLog,
    [int]$ExpectedFps,
    [int]$WarmupSec = 10
) {
    if (-not (Test-Path $ViewerLog)) {
        return @{ Passed = $false; Detail = "missing viewer log" }
    }

    $lines = Get-Content $ViewerLog -ErrorAction SilentlyContinue
    $firstKf = $lines | Select-String -Pattern "mono_ms=(\d+) event=first_keyframe" | Select-Object -First 1
    if (-not $firstKf) {
        $firstKf = $lines | Select-String -Pattern "event=first_keyframe mono_ms=(\d+)" | Select-Object -First 1
    }
    if (-not $firstKf) {
        return @{ Passed = $false; Detail = "no first_keyframe event" }
    }
    $firstKfMs = [int]$firstKf.Matches[0].Groups[1].Value
    if ($firstKfMs -gt 3000) {
        return @{ Passed = $false; Detail = "first keyframe at ${firstKfMs}ms (>3000ms)" }
    }

    $tickPattern = "viewer_probe_tick .* mono_ms=(\d+) .* dec_fps=([\d.]+).* native_fps=([\d.]+).* decode_stall_ms=(\d+)"
    $decFpsSamples = @()
    $minFps = [double]::MaxValue
    $maxStall = 0
    $maxIncomplete = 0
    $lastNativeRtp = $null

    foreach ($line in $lines) {
        if ($line -match $tickPattern) {
            $monoMs = [int]$Matches[1]
            if ($monoMs -lt ($WarmupSec * 1000)) { continue }
            $decFps = [double]$Matches[2]
            $nativeFps = [double]$Matches[3]
            $fps = if ($decFps -gt 0) { $decFps } else { $nativeFps }
            $stall = [int]$Matches[4]
            $decFpsSamples += $fps
            if ($fps -lt $minFps) { $minFps = $fps }
            if ($stall -gt $maxStall) { $maxStall = $stall }
        }
        if ($line -match "viewer_probe_native_rtp .* rx_incomplete=(\d+)") {
            $lastNativeRtp = [int]$Matches[1]
        }
    }
    if ($null -ne $lastNativeRtp) {
        $maxIncomplete = $lastNativeRtp
    }

    if ($decFpsSamples.Count -lt 5) {
        return @{ Passed = $false; Detail = "insufficient dec_fps samples after warmup" }
    }

    $avgFps = ($decFpsSamples | Measure-Object -Average).Average
    $minTarget = [math]::Max(45, $ExpectedFps - 5)
    $failures = @()
    if ($avgFps -lt $minTarget) { $failures += "avg dec_fps=$([math]::Round($avgFps,1)) < $minTarget" }
    if ($minFps -lt 45) { $failures += "min dec_fps=$([math]::Round($minFps,1)) < 45" }
    if ($maxStall -gt 500) { $failures += "max decode_stall_ms=$maxStall > 500" }
    if ($maxIncomplete -gt 0) { $failures += "rx_incomplete=$maxIncomplete" }

    if ($failures.Count -gt 0) {
        return @{ Passed = $false; Detail = ($failures -join "; ") }
    }
    return @{
        Passed = $true
        Detail = "first_kf=${firstKfMs}ms avg_fps=$([math]::Round($avgFps,1)) min_fps=$([math]::Round($minFps,1)) max_stall_ms=$maxStall"
    }
}

function Test-HostCertLog([string]$HostLog) {
    if (-not (Test-Path $HostLog)) {
        return @{ Passed = $false; Detail = "missing host log" }
    }
    $lines = Get-Content $HostLog -ErrorAction SilentlyContinue
    $recoveryLines = @($lines | Select-String -Pattern "recovery_mode=true")
    $backpressure = @($lines | Select-String -Pattern "reason=rtp_send_backpressure")
    if ($recoveryLines.Count -gt 0) {
        return @{ Passed = $false; Detail = "recovery_mode active ($($recoveryLines.Count) samples)" }
    }
    if ($backpressure.Count -gt 0) {
        return @{ Passed = $false; Detail = "rtp_send_backpressure logged" }
    }
    return @{ Passed = $true; Detail = "no recovery/backpressure during run" }
}

function Stop-StreamProbeProcesses([int[]]$ExtraPids = @()) {
    foreach ($pid in $ExtraPids) {
        Stop-Process -Id $pid -Force -ErrorAction SilentlyContinue
    }
    Start-Sleep -Seconds 1
}

function Invoke-LanVisualGate(
    [string]$Name,
    [int]$Width,
    [int]$Height,
    [int]$Fps,
    [int]$BitrateKbps
) {
    $hostLog = Join-Path $CertDir "$Name-host.log"
    $viewerLog = Join-Path $CertDir "$Name-viewer.log"
    if (Test-Path $hostLog) { Remove-Item $hostLog -Force }
    if (Test-Path $viewerLog) { Remove-Item $viewerLog -Force }

    $hostJob = $null
    $viewerExit = 1
    try {
        Stop-StreamProbeProcesses
        $hostJob = Start-Job -ScriptBlock {
            param($Root, $TargetDir, $Token, $CrewId, $Base, $Fps, $Bitrate, $Width, $Height, $Title, $Log, $SourceTitle)
            $env:CARGO_TARGET_DIR = $TargetDir
            Set-Location $Root
            $args = @(
                "run", "--release", "-p", "stream-host", "--",
                "--fps", $Fps,
                "--bitrate", $Bitrate,
                "--nakama-start-stream",
                "--nakama-http-base", $Base,
                "--nakama-auth-token", $Token,
                "--crew-id", $CrewId,
                "--stream-title", $Title,
                "--request-width", $Width,
                "--request-height", $Height,
                "--source-title-substring", $SourceTitle
            )
            & cargo @args 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $Log
        } -ArgumentList @(
            $repoRoot,
            $env:CARGO_TARGET_DIR,
            $probeCtx.AuthToken,
            $probeCtx.CrewId,
            $probeCtx.NakamaHttpBase,
            $Fps,
            $BitrateKbps,
            $Width,
            $Height,
            "Cert $Name",
            $hostLog,
            $CaptureSourceTitle
        )

        $session = Wait-ForHostSession -HostLog $hostLog -TimeoutSec 120
        if (-not $session) {
            Add-GateResult $Name $false "host session not started (log: $hostLog)"
            return
        }

        Push-Location $repoRoot
        & (Join-Path $PSScriptRoot "run-stream-viewer.ps1") `
            -Session $session `
            -HostLog $hostLog `
            -ViewerLog $viewerLog `
            -NativeMetrics $true `
            -HoldSec $LanHoldSec `
            -Width $Width `
            -Height $Height
        $viewerExit = $LASTEXITCODE
        Pop-Location
    }
    finally {
        if ($hostJob) {
            Stop-Job $hostJob -ErrorAction SilentlyContinue
            Remove-Job $hostJob -Force -ErrorAction SilentlyContinue
        }
    }

    $viewerResult = Test-ViewerCertLog -ViewerLog $viewerLog -ExpectedFps $Fps
    $hostResult = Test-HostCertLog -HostLog $hostLog
    $passed = ($viewerExit -eq 0) -and $viewerResult.Passed -and $hostResult.Passed
    $detail = "viewer: $($viewerResult.Detail); host: $($hostResult.Detail)"
    Add-GateResult $Name $passed $detail
}

Write-Host ""
Write-Host "=== Mello RTP Stream Certification ===" -ForegroundColor Cyan
Write-Host "Output: $CertDir" -ForegroundColor DarkGray
Write-Host ""

Test-Prerequisites

if (-not $SkipSoak) {
    Write-Host "`n--- SFU synthetic soak ---" -ForegroundColor Yellow
    Invoke-StreamSoakGate "soak_5_viewers_30s" 5 30000 0.99 90.0
    Invoke-StreamSoakGate "soak_20_viewers_90s" 20 90000 0.99 90.0
    Invoke-StreamSoakGate "soak_32_viewers_90s" 32 90000 0.985 130.0
}

if (-not $SkipLan) {
    Write-Host "`n--- LAN visual (headless SFU) ---" -ForegroundColor Yellow
    Invoke-LanVisualGate "lan_720p60_sfu" 1280 720 60 4000
    Stop-StreamProbeProcesses
    Start-Sleep -Seconds 3
    Invoke-LanVisualGate "lan_1080p60_sfu" 1920 1080 60 6000
}

if (-not $SkipLoss) {
    Write-Host "`n--- Loss / WAN ---" -ForegroundColor Yellow
    Write-Host "[SKIP] loss_wan_matrix — requires external packet-loss emulator" -ForegroundColor DarkYellow
}

if (-not $SkipProductClient) {
    Write-Host "`n--- Product client ---" -ForegroundColor Yellow
    Write-Host "[SKIP] product_client_dcomp — manual DComp client smoke" -ForegroundColor DarkYellow
}

$passedCount = @($results | Where-Object { $_.Passed }).Count
$failed = @($results | Where-Object { -not $_.Passed })

Write-Host ""
Write-Host "=== Summary: $passedCount/$($results.Count) gates passed ===" -ForegroundColor Cyan
if ($failed.Count -gt 0) {
    Write-Host "Failed gates:" -ForegroundColor Red
    $failed | ForEach-Object { Write-Host "  - $($_.Gate): $($_.Detail)" }
    exit 1
}

exit 0
