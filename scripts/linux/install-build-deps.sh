#!/usr/bin/env bash
# System packages for building and testing the Linux client on Ubuntu 22.04.
# One list for the build container (scripts/linux/Dockerfile) and the CI job
# (.github/workflows/linux-build-test.yml). Run as root.
#
# vcpkg, libmello (CMake, C++20 island needs GCC 12), bindgen (libclang),
# Skia and winit (fontconfig, freetype, X11, Wayland, EGL), the client crates
# (OpenSSL for native-tls, D-Bus for the keyring and the portals, GTK and
# appindicator for rfd, Xtst for rdev, PipeWire for audio), the GStreamer
# headers for the VA-API stream decoder (headers only: libmello loads
# GStreamer at run time). rsync copies the source into the container.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

apt-get update
# GitHub's ubuntu-22.04 image ships libunwind-14-dev (with its LLVM 14), which
# conflicts with libunwind-dev, a dependency of libgstreamer1.0-dev. Nothing
# here uses LLVM's libunwind; the build container never has it.
if dpkg -s libunwind-14-dev >/dev/null 2>&1; then
    apt-get remove -y libunwind-14-dev
fi
apt-get install -y --no-install-recommends \
    build-essential gcc-12 g++-12 cmake ninja-build pkg-config \
    curl ca-certificates zip unzip tar git perl python3 \
    autoconf automake libtool rsync file \
    libclang-dev clang \
    libfontconfig-dev libfreetype-dev \
    libx11-dev libx11-xcb-dev libxcursor-dev libxi-dev libxrandr-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libxtst-dev libxdo-dev \
    libwayland-dev libegl-dev libgl-dev \
    libssl-dev libdbus-1-dev \
    libgtk-3-dev libayatana-appindicator3-dev \
    libpipewire-0.3-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev
rm -rf /var/lib/apt/lists/*

# GCC 12 as the default C and C++ compiler (Ubuntu 22.04 defaults to 11).
update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-12 120 \
    --slave /usr/bin/g++ g++ /usr/bin/g++-12
update-alternatives --install /usr/bin/cc cc /usr/bin/gcc-12 120
update-alternatives --install /usr/bin/c++ c++ /usr/bin/g++-12 120
