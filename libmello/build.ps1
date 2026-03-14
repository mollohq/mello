# build.ps1 — Standalone CMake build for libmello (Windows)
# Usage: .\build.ps1 [-Clean] [-Release]

param(
    [switch]$Clean,
    [switch]$Release
)

$ErrorActionPreference = "Stop"
$buildDir = "$PSScriptRoot\build"
$vcpkgRoot = "$PSScriptRoot\..\external\vcpkg"
$triplet = "x64-windows-static-md"
$config = if ($Release) { "Release" } else { "Debug" }

# Bootstrap vcpkg if needed
if (-not (Test-Path "$vcpkgRoot\vcpkg.exe")) {
    Write-Host "[build] Bootstrapping vcpkg..." -ForegroundColor Cyan
    & "$vcpkgRoot\bootstrap-vcpkg.bat" -disableMetrics
    if ($LASTEXITCODE -ne 0) { throw "vcpkg bootstrap failed" }
}

if ($Clean -and (Test-Path $buildDir)) {
    Write-Host "[build] Cleaning build directory..." -ForegroundColor Yellow
    Remove-Item -Recurse -Force $buildDir
}

if (-not (Test-Path $buildDir)) {
    New-Item -ItemType Directory -Path $buildDir | Out-Null
}

Write-Host "[build] Configuring ($config, triplet=$triplet)..." -ForegroundColor Cyan
cmake -S $PSScriptRoot -B $buildDir `
    "-DCMAKE_TOOLCHAIN_FILE=$vcpkgRoot\scripts\buildsystems\vcpkg.cmake" `
    "-DVCPKG_TARGET_TRIPLET=$triplet" `
    "-DCMAKE_BUILD_TYPE=$config" `
    "-DMELLO_BUILD_TESTS=OFF"

if ($LASTEXITCODE -ne 0) { throw "CMake configure failed" }

Write-Host "[build] Building..." -ForegroundColor Cyan
cmake --build $buildDir --config $config -- /maxcpucount

if ($LASTEXITCODE -ne 0) { throw "CMake build failed" }

Write-Host "[build] Success! Output in $buildDir\$config" -ForegroundColor Green
