# Drive a *built* Mello binary through a real signup and fail if it cannot.
#
# PowerShell twin of run-signup-smoke.sh. Windows needs its own copy because
# `shell: bash` on a self-hosted Windows runner can resolve to WSL bash
# (C:\Windows\system32\bash.exe), which cannot read a Windows script path and
# fails with "No such file or directory" before the script ever starts.
#
# This is the only check that exercises the shipped artifact rather than the
# source. Keys such as NAKAMA_HTTP_KEY are baked in at compile time, so a build
# made with the wrong secret is indistinguishable from a correct one until a
# user tries to sign up.
#
# Usage:
#   scripts\run-signup-smoke.ps1 <path-to-mello-binary>
#
# Exit 0 = a new user can sign up with this binary.

param(
    [Parameter(Mandatory = $true)]
    [string]$Binary,

    # Upper bound on the whole scenario. Without it a client that never reaches
    # its last step would hold the release job until the job timeout.
    [int]$TimeoutSeconds = 180
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Binary)) {
    Write-Error "binary not found: $Binary"
    exit 2
}
$binaryPath = (Resolve-Path -LiteralPath $Binary).Path

$repoRoot = Split-Path -Parent $PSScriptRoot
$scenario = Join-Path $repoRoot 'tools\perf-harness\scenarios\signup_smoke.json'
if (-not (Test-Path -LiteralPath $scenario)) {
    Write-Error "scenario not found: $scenario"
    exit 2
}

$signalDir = Join-Path ([System.IO.Path]::GetTempPath()) ("mello-smoke-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $signalDir -Force | Out-Null

try {
    # Unique crew name per run so repeated runs never collide.
    $env:MELLO_SMOKE_ID = "$([int][double]::Parse((Get-Date -UFormat %s)))-$PID"

    # Isolated config dir: the smoke run must not read or clobber whatever
    # onboarding state exists on the machine. A runner with completed
    # onboarding persisted would otherwise skip the very flow under test.
    $configDir = Join-Path $signalDir 'config'
    New-Item -ItemType Directory -Path $configDir -Force | Out-Null
    $env:MELLO_CONFIG_DIR = $configDir

    $env:MELLO_PERF_MODE = '1'
    $env:MELLO_PERF_SCENARIO = $scenario
    $env:MELLO_PERF_SIGNAL_DIR = $signalDir
    if (-not $env:RUST_LOG) { $env:RUST_LOG = 'info' }

    Write-Host "> signup smoke: $Binary"
    Write-Host "  scenario: $scenario"

    # Start-Process + WaitForExit, not `& $Binary`.
    #
    # Release builds set `windows_subsystem = "windows"` (client/src/main.rs),
    # and PowerShell does not wait for a GUI-subsystem executable invoked with
    # the call operator — it returns as soon as the process is launched. The
    # script then read done.json before the client had written it and failed a
    # run that was actually fine. Bash waits for any child regardless of
    # subsystem, which is why the macOS twin never hit this.
    #
    # -NoNewWindow keeps the client's stdio on this console so its log still
    # streams into the CI output. The path must be absolute: Start-Process
    # resolves a relative one against the process working directory rather than
    # PowerShell's current location, and CI passes target\release\mello.exe.
    $proc = Start-Process -FilePath $binaryPath -NoNewWindow -PassThru

    if (-not $proc.WaitForExit($TimeoutSeconds * 1000)) {
        Write-Host "X signup smoke FAILED: client still running after $TimeoutSeconds s."
        try { $proc.Kill($true) } catch { }
        exit 1
    }

    $done = Join-Path $signalDir 'done.json'
    if (-not (Test-Path -LiteralPath $done)) {
        Write-Host "X signup smoke FAILED: the client exited without reporting a result."
        Write-Host "  It likely crashed or quit before the scenario finished."
        exit 1
    }

    $result = Get-Content -LiteralPath $done -Raw
    Write-Host "  result: $result"

    # The scenario runner reports through this file rather than the process exit
    # code, so the status has to be translated here or a failure would pass CI
    # silently.
    if ($result -match '"status"\s*:\s*"ok"') {
        Write-Host "+ signup smoke passed: this binary can create a new account."
        exit 0
    }

    Write-Host "X signup smoke FAILED - do not release this build."
    Write-Host "  A new user installing it would not be able to sign up."
    Write-Host "  Most likely: NAKAMA_HTTP_KEY baked into the build does not match the server."
    exit 1
}
finally {
    Remove-Item -LiteralPath $signalDir -Recurse -Force -ErrorAction SilentlyContinue
}
