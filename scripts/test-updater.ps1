#!/usr/bin/env pwsh
# Test the Velopack auto-updater locally without code signing.
#
# Usage:
#   .\scripts\test-updater.ps1 -Action scenario           # pack/install v0.1.0, pack v0.2.0, launch
#   .\scripts\test-updater.ps1                            # pack v0.1.0 (default)
#   .\scripts\test-updater.ps1 -Version 0.2.0             # pack v0.2.0
#   .\scripts\test-updater.ps1 -Action install            # install matching -Version from vpk-out
#   .\scripts\test-updater.ps1 -Action launch             # launch installed app with local update source
#   .\scripts\test-updater.ps1 -Action clean              # delete local dist/vpk-out test artifacts
#
# Typical workflow:
#   .\scripts\test-updater.ps1 -Action scenario
#
# Manual workflow:
#   1. .\scripts\test-updater.ps1 -Action clean
#   2. .\scripts\test-updater.ps1 -Version 0.1.0          # build & pack v1
#   3. .\scripts\test-updater.ps1 -Action install -Version 0.1.0
#   4. .\scripts\test-updater.ps1 -Version 0.2.0          # build & pack v2
#   5. .\scripts\test-updater.ps1 -Action launch           # v1 detects v2

param(
    [ValidateSet("pack", "install", "launch", "scenario", "clean")]
    [string]$Action  = "pack",
    [string]$Version = "0.1.0",
    [string]$OldVersion = "0.1.0",
    [string]$NewVersion = "0.2.0",
    [int]$ApplyDelaySeconds = 10,
    [switch]$NoRestoreVersion
)

$ErrorActionPreference = "Stop"

$RepoRoot   = (Resolve-Path "$PSScriptRoot\..").Path
$DistDir    = Join-Path $RepoRoot "dist"
$VpkOut     = Join-Path $RepoRoot "vpk-out"
$Channel    = "win-x64-dev"
$OrtDir     = Join-Path $RepoRoot "libmello\third_party\onnxruntime\onnxruntime-win-x64-1.23.2\lib"
$CargoToml  = Join-Path $RepoRoot "Cargo.toml"

Write-Host ""
Write-Host "  mello updater test" -ForegroundColor Cyan
Write-Host "  ------------------"
Write-Host ""

function Assert-Vpk {
    if (-not (Get-Command vpk -ErrorAction SilentlyContinue)) {
        Write-Host "  [!] vpk not found. Install it:" -ForegroundColor Red
        Write-Host "      dotnet tool install -g vpk" -ForegroundColor DarkGray
        exit 1
    }
}

function Get-WorkspaceVersion {
    $content = Get-Content $CargoToml -Raw
    if ($content -match '(?m)^version\s*=\s*"([^"]+)"') {
        return $Matches[1]
    }
    throw "Could not read workspace package version from $CargoToml"
}

function Set-WorkspaceVersion([string]$TargetVersion) {
    $content = Get-Content $CargoToml -Raw
    $updated = $content -replace '(?m)^(version\s*=\s*")[^"]+(")', "`${1}$TargetVersion`$2"
    Set-Content $CargoToml -Value $updated -NoNewline
}

function Invoke-WithWorkspaceVersion([string]$BuildVersion, [scriptblock]$Body) {
    $originalVersion = Get-WorkspaceVersion
    Set-WorkspaceVersion $BuildVersion

    try {
        & $Body
    } finally {
        if (-not $NoRestoreVersion) {
            Set-WorkspaceVersion $originalVersion
        }
    }
}

# ── clean ─────────────────────────────────────────────────────────

function Do-Clean {
    Write-Host "  Cleaning local updater artifacts..." -ForegroundColor Yellow
    if (Test-Path $DistDir) { Remove-Item $DistDir -Recurse -Force }
    if (Test-Path $VpkOut) { Remove-Item $VpkOut -Recurse -Force }
    Write-Host "  done." -ForegroundColor Green
    Write-Host ""
}

# ── pack ──────────────────────────────────────────────────────────

function Do-Pack([string]$PackVersion) {
    Assert-Vpk

    Write-Host "  [1/3] Building client (release) v$PackVersion..." -ForegroundColor Yellow

    Invoke-WithWorkspaceVersion $PackVersion {
        Push-Location $RepoRoot
        try {
            cargo build --release -p mello-client
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
        } finally { Pop-Location }

        Write-Host "  [2/3] Assembling dist..." -ForegroundColor Yellow
        if (Test-Path $DistDir) { Remove-Item $DistDir -Recurse -Force }
        New-Item $DistDir -ItemType Directory -Force | Out-Null

        Copy-Item (Join-Path $RepoRoot "target\release\mello.exe") $DistDir

        @("onnxruntime.dll", "onnxruntime_providers_shared.dll") | ForEach-Object {
            $src = Join-Path $OrtDir $_
            if (Test-Path $src) {
                Copy-Item $src $DistDir
            } else {
                $fallback = Join-Path $RepoRoot "target\release\deps\$_"
                if (Test-Path $fallback) {
                    Copy-Item $fallback $DistDir
                } else {
                    Write-Host "    [warn] $_ not found, skipping" -ForegroundColor DarkYellow
                }
            }
        }

        # Silero VAD model — libmello looks for it next to the exe
        $vadModel = Get-ChildItem (Join-Path $RepoRoot "target\release\build") -Recurse -Filter "silero_vad.onnx" | Select-Object -First 1
        if ($vadModel) {
            Copy-Item $vadModel.FullName $DistDir
        } else {
            Write-Host "    [warn] silero_vad.onnx not found in build output" -ForegroundColor DarkYellow
        }

        Write-Host "  [3/3] Packing v$PackVersion (channel: $Channel)..." -ForegroundColor Yellow
        if (-not (Test-Path $VpkOut)) { New-Item $VpkOut -ItemType Directory -Force | Out-Null }

        vpk pack `
            --packId Mello `
            --packVersion $PackVersion `
            --packDir $DistDir `
            --mainExe mello.exe `
            --packTitle Mello `
            --channel $Channel `
            --outputDir $VpkOut `
            --icon (Join-Path $RepoRoot "client\assets\icons\mello.ico")

        if ($LASTEXITCODE -ne 0) { throw "vpk pack failed" }
    }

    Write-Host ""
    Write-Host "  done! Packed v$PackVersion -> vpk-out/" -ForegroundColor Green
    if (-not $NoRestoreVersion) {
        Write-Host "  restored workspace version to $(Get-WorkspaceVersion)" -ForegroundColor DarkGray
    }
    Write-Host ""
    Write-Host "  Next steps:" -ForegroundColor DarkGray
    Write-Host "    Install:  .\scripts\test-updater.ps1 -Action install -Version $PackVersion" -ForegroundColor DarkGray
    Write-Host "    Launch:   .\scripts\test-updater.ps1 -Action launch" -ForegroundColor DarkGray
    Write-Host ""
}

# ── install ───────────────────────────────────────────────────────

function Do-Install([string]$InstallVersion) {
    $setup = Get-ChildItem $VpkOut -Filter "*Setup*.exe" -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like "*$InstallVersion*" -and $_.Name -like "*$Channel*" } |
        Sort-Object Name -Descending |
        Select-Object -First 1

    if (-not $setup) {
        $setup = Get-ChildItem $VpkOut -Filter "*-$Channel-Setup.exe" -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            Select-Object -First 1
    }
    if (-not $setup) {
        Write-Host "  [!] No Setup.exe found in vpk-out/. Run pack first." -ForegroundColor Red
        exit 1
    }

    Write-Host "  Installing $($setup.Name)..." -ForegroundColor Yellow
    Start-Process $setup.FullName -Wait

    Write-Host ""
    Write-Host "  done! Mello installed." -ForegroundColor Green
    Write-Host ""
    Write-Host "  Launch with:  .\scripts\test-updater.ps1 -Action launch" -ForegroundColor DarkGray
    Write-Host ""
}

# ── launch ────────────────────────────────────────────────────────

function Do-Launch {
    $exe = Join-Path $env:LOCALAPPDATA "Mello\current\mello.exe"
    if (-not (Test-Path $exe)) {
        Write-Host "  [!] Mello not installed at $exe" -ForegroundColor Red
        Write-Host "      Run:  .\scripts\test-updater.ps1 -Action install" -ForegroundColor DarkGray
        exit 1
    }

    $env:MELLO_UPDATE_URL = $VpkOut
    $env:RUST_LOG = "debug"
    $env:MELLO_UPDATE_APPLY_DELAY_MS = [string]($ApplyDelaySeconds * 1000)

    Write-Host "  Launching installed Mello..." -ForegroundColor Yellow
    Write-Host "  MELLO_UPDATE_URL = $VpkOut" -ForegroundColor DarkGray
    Write-Host "  RUST_LOG         = debug" -ForegroundColor DarkGray
    Write-Host "  Apply delay      = ${ApplyDelaySeconds}s" -ForegroundColor DarkGray
    Write-Host "  Expectation      = force-update window if installed version is older than vpk-out latest" -ForegroundColor DarkGray
    Write-Host ""

    & $exe
}

# ── scenario ──────────────────────────────────────────────────────

function Do-Scenario {
    Write-Host "  Force-update scenario: v$OldVersion -> v$NewVersion" -ForegroundColor Cyan
    Write-Host ""
    Do-Clean
    Do-Pack $OldVersion
    Do-Install $OldVersion
    Do-Pack $NewVersion
    Do-Launch
}

# ── dispatch ──────────────────────────────────────────────────────

switch ($Action) {
    "pack"     { Do-Pack $Version }
    "install"  { Do-Install $Version }
    "launch"   { Do-Launch }
    "scenario" { Do-Scenario }
    "clean"    { Do-Clean }
}
