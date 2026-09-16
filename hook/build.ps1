# Builds the game capture hook set.
#
#   .\build.ps1            # 64-bit, Release
#   .\build.ps1 -Bits 32   # 32-bit set, for 32-bit games
#
# The hook is a separate project from libmello because it builds for both
# architectures and links the static CRT.
param(
    [ValidateSet(32, 64)][int]$Bits = 64,
    [ValidateSet("Debug", "Release")][string]$Config = "Release"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $root
$vcpkg = Join-Path $repo "external/vcpkg"
if (-not (Test-Path $vcpkg)) { throw "external/vcpkg not found - run: git submodule update --init" }

$arch = if ($Bits -eq 64) { "x64" } else { "x86" }
$triplet = "$arch-windows-static"
$build = Join-Path $root "build/$arch"

cmake -S $root -B $build -A $(if ($Bits -eq 64) { "x64" } else { "Win32" }) `
    "-DCMAKE_TOOLCHAIN_FILE=$vcpkg/scripts/buildsystems/vcpkg.cmake" `
    "-DVCPKG_TARGET_TRIPLET=$triplet" `
    "-DVCPKG_OVERLAY_TRIPLETS=$root/triplets" `
    "-DCMAKE_BUILD_TYPE=$Config"
if ($LASTEXITCODE -ne 0) { throw "configure failed" }

cmake --build $build --config $Config
if ($LASTEXITCODE -ne 0) { throw "build failed" }

Write-Host "[mello-hook] $Bits-bit set in $build/$Config"
