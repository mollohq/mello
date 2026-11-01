# Build environment for the Linux client.
#
# Ubuntu 24.04, as GitHub's runner for the release: its glibc 2.39 is the
# oldest glibc the Linux release supports. A binary built here runs on every
# distribution with glibc 2.39 or newer.
# The same image builds on a Mac (Docker with Rosetta) and on a Linux host.
FROM --platform=linux/amd64 ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# The system packages: the same list as the CI job.
COPY install-build-deps.sh /tmp/install-build-deps.sh
RUN /tmp/install-build-deps.sh && rm /tmp/install-build-deps.sh

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
# Pinned to the toolchain on the developer Mac: a newer clippy brings new
# lints, and check.sh must give the same answer here as there. The CI job
# (.github/workflows/linux-build-test.yml) pins the same version.
ARG RUST_VERSION=1.94.0
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
        --default-toolchain "$RUST_VERSION" --component rustfmt,clippy \
    && rustc --version

# Velopack packaging: vpk runs on .NET 8, builds the AppImage with
# squashfs-tools and the delta packages with zstd. The vpk version matches the
# velopack crate in Cargo.lock. 0.0.1298 embeds the AppImageKit runtime, which
# loads libfuse.so.2 at start; 1.1.1 and later have the static type-2 runtime
# (feat/velopack-1.2), and their delta packages need zstd.
ARG VPK_VERSION=0.0.1298
RUN apt-get update && apt-get install -y --no-install-recommends \
        dotnet-sdk-8.0 squashfs-tools zstd \
    && rm -rf /var/lib/apt/lists/* \
    && dotnet tool install vpk --version "$VPK_VERSION" --tool-path /opt/vpk \
    && /opt/vpk/vpk --help >/dev/null
ENV PATH=/opt/vpk:$PATH DOTNET_CLI_TELEMETRY_OPTOUT=1

# Smoke tests of the built client without a display: a virtual X server.
RUN apt-get update && apt-get install -y --no-install-recommends xvfb xauth \
    && rm -rf /var/lib/apt/lists/*

