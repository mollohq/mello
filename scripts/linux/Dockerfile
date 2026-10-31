# Build environment for the Linux client.
#
# Ubuntu 22.04 on purpose: its glibc 2.35 is the oldest glibc the Linux
# release supports. A binary built here runs on every newer distribution.
# The same image builds on a Mac (Docker with Rosetta) and on a Linux host.
FROM --platform=linux/amd64 ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

# vcpkg, libmello (CMake, C++20 island needs GCC 12), bindgen (libclang),
# Skia and winit (fontconfig, freetype, X11, Wayland, EGL), the client crates
# (OpenSSL for native-tls, D-Bus for the keyring, GTK and appindicator for the
# tray and file dialog, Xtst for rdev). rsync copies the source in.
RUN apt-get update && apt-get install -y --no-install-recommends \
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
    && rm -rf /var/lib/apt/lists/*

RUN update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-12 120 \
        --slave /usr/bin/g++ g++ /usr/bin/g++-12 \
    && update-alternatives --install /usr/bin/cc cc /usr/bin/gcc-12 120 \
    && update-alternatives --install /usr/bin/c++ c++ /usr/bin/g++-12 120

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
        --default-toolchain stable --component rustfmt,clippy \
    && rustc --version

WORKDIR /src
