# MELLO Getting Started Guide

> **Purpose:** Set up your development environment for Mello  
> **Time:** ~1-2 hours  
> **OS:** Windows 10/11 (primary), with notes for macOS/Linux

---

## Quick Start (TL;DR)

```powershell
# 1. Install prerequisites (see Section 2)
# 2. Clone and setup:
git clone https://github.com/mello-app/mello.git
cd mello

# 3. Start backend
cd backend
docker compose up -d

# 4. Build client (in another terminal)
cd client
cargo run
```

---

## Table of Contents

1. [Prerequisites](#1-prerequisites)
2. [Installing Tools](#2-installing-tools)
3. [Backend Setup (Nakama)](#3-backend-setup-nakama)
4. [Client Setup (Rust/Slint)](#4-client-setup-rustslint)
5. [libmello Setup (C++)](#5-libmello-setup-c)
6. [IDE Configuration](#6-ide-configuration)
7. [Running Everything](#7-running-everything)
8. [Troubleshooting](#8-troubleshooting)
9. [Development Workflow](#9-development-workflow)

---

## 1. Prerequisites

### Required Software

| Tool | Version | Purpose |
|------|---------|---------|
| Git | Latest | Version control |
| Docker Desktop | Latest | Backend services |
| Rust | 1.75+ | Client & mello-core |
| Visual Studio 2022 | Latest | C++ compiler & Windows SDK |
| CMake | 3.20+ | C++ build system |
| Node.js | 18+ | Tooling (optional) |

### Hardware Requirements

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| RAM | 8GB | 16GB |
| Disk | 20GB free | 50GB free |
| GPU | Any (for encode testing) | NVIDIA GTX 1060+ |

### Windows SDK Components

You need these Windows SDK components (install via Visual Studio Installer):
- Windows 10/11 SDK (latest)
- C++ build tools
- C++ CMake tools

---

## 2. Installing Tools

### 2.1 Git

```powershell
# Using winget
winget install Git.Git

# Verify
git --version
```

### 2.2 Docker Desktop

1. Download from https://www.docker.com/products/docker-desktop
2. Install and restart
3. Enable WSL 2 backend (recommended)

```powershell
# Verify
docker --version
docker compose version
```

### 2.3 Rust

```powershell
# Install rustup
winget install Rustlang.Rustup

# Or download from https://rustup.rs

# After install, open new terminal:
rustup default stable
rustup update

# Verify
rustc --version
cargo --version
```

### 2.4 Visual Studio 2022

1. Download Visual Studio 2022 Community: https://visualstudio.microsoft.com/
2. In installer, select:
   - **Workloads:**
     - "Desktop development with C++"
   - **Individual Components:**
     - Windows 10/11 SDK (latest)
     - C++ CMake tools for Windows
     - MSVC v143 build tools

### 2.5 CMake

```powershell
# If not installed with VS:
winget install Kitware.CMake

# Add to PATH if needed
# Verify
cmake --version
```

### 2.6 LLVM/Clang (for bindgen)

```powershell
winget install LLVM.LLVM

# Add to PATH: C:\Program Files\LLVM\bin
# Set environment variable:
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\lib"

# Add to your PowerShell profile or system environment variables
```

### 2.7 vcpkg (C++ package manager)

```powershell
cd C:\
git clone https://github.com/Microsoft/vcpkg.git
cd vcpkg
.\bootstrap-vcpkg.bat

# Integrate with MSBuild/CMake
.\vcpkg integrate install

# Set environment variable
$env:VCPKG_ROOT = "C:\vcpkg"
```

---

## 3. Backend Setup (Nakama)

### 3.1 Clone Repository

```powershell
git clone https://github.com/mello-app/mello.git
cd mello
```

### 3.2 Configure Environment

```powershell
cd backend

# Copy example environment file
cp .env.example .env

# Edit .env with your values (or use defaults for local dev)
```

**.env file contents:**

```env
# Nakama
NAKAMA_HTTP_KEY=mello_http_key_dev

# Discord OAuth (optional for local dev)
DISCORD_CLIENT_ID=your_discord_client_id
DISCORD_CLIENT_SECRET=your_discord_client_secret

# TURN server (optional for local dev)
TURN_SECRET=mello_turn_secret_dev
TURN_HOST=localhost
```

### 3.3 Start Backend Services

```powershell
# Start all services (Nakama + PostgreSQL)
docker compose up -d

# Check status
docker compose ps

# View logs
docker compose logs -f nakama
```

### 3.4 Verify Backend

Open browser: http://localhost:7351

- Username: `admin`
- Password: `mello_admin_dev`

You should see the Nakama admin console.

### 3.5 Backend Commands

```powershell
# Stop services
docker compose down

# Stop and remove data
docker compose down -v

# Restart Nakama only
docker compose restart nakama

# View Nakama logs
docker compose logs -f nakama

# Access PostgreSQL
docker compose exec postgres psql -U nakama -d nakama
```

---

## 4. Client Setup (Rust/Slint)

### 4.1 Build Client

```powershell
cd client

# First build (downloads dependencies)
cargo build

# Run in debug mode
cargo run

# Build release
cargo build --release
```

### 4.2 Slint UI Development

```powershell
# Install Slint VS Code extension for .slint file support
code --install-extension slint.slint

# Or install Slint LSP manually
cargo install slint-lsp
```

### 4.3 Live UI Preview

The Slint VS Code extension provides live preview. Alternatively:

```powershell
# Install slint-viewer
cargo install slint-viewer

# Preview a .slint file
slint-viewer client/ui/main.slint
```

### 4.4 Client Directory Structure

```
client/
├── Cargo.toml          # Rust dependencies
├── build.rs            # Slint compilation
├── src/
│   └── main.rs         # Entry point
└── ui/
    ├── main.slint      # Root UI
    └── components/     # UI components
```

---

## 5. libmello Setup (C++)

### 5.1 Install Dependencies

```powershell
cd libmello

# Using vcpkg
vcpkg install opus:x64-windows
vcpkg install openssl:x64-windows

# RNNoise (manual build)
git clone https://github.com/xiph/rnnoise.git deps/rnnoise
cd deps/rnnoise
# Build with CMake or use pre-built

# libdatachannel
git clone https://github.com/paullouisageneau/libdatachannel.git deps/libdatachannel
cd deps/libdatachannel
git submodule update --init --recursive
```

### 5.2 Download Silero VAD

```powershell
# Download ONNX model
mkdir -p deps/silero-vad
cd deps/silero-vad

# Download from: https://github.com/snakers4/silero-vad/raw/master/files/silero_vad.onnx
curl -L -o silero_vad.onnx https://github.com/snakers4/silero-vad/raw/master/files/silero_vad.onnx

# Download ONNX Runtime
# From: https://github.com/microsoft/onnxruntime/releases
# Extract to deps/onnxruntime
```

### 5.3 Configure CMake

```powershell
cd libmello

# Create build directory
mkdir build
cd build

# Configure
cmake .. -G "Visual Studio 17 2022" -A x64 `
    -DCMAKE_TOOLCHAIN_FILE="C:/vcpkg/scripts/buildsystems/vcpkg.cmake" `
    -DMELLO_ENABLE_NVENC=ON `
    -DMELLO_ENABLE_AMF=ON `
    -DMELLO_ENABLE_QSV=ON

# Build
cmake --build . --config Release

# Or open in Visual Studio
start mello.sln
```

### 5.4 NVIDIA SDK Setup (for NVENC)

1. Download NVIDIA Video Codec SDK: https://developer.nvidia.com/nvidia-video-codec-sdk
2. Extract to `C:\Program Files\NVIDIA GPU Computing Toolkit\VideoCodecSDK`
3. Set environment variable:

```powershell
$env:NVENC_SDK_PATH = "C:\Program Files\NVIDIA GPU Computing Toolkit\VideoCodecSDK"
```

### 5.5 AMD AMF Setup

1. Download AMD AMF SDK: https://github.com/GPUOpen-LibrariesAndSDKs/AMF
2. Clone or extract to `deps/amf`

### 5.6 FFI Bindings (mello-sys)

```powershell
cd mello-sys

# Generate bindings from mello.h
# This happens automatically via build.rs
cargo build
```

---

## 6. IDE Configuration

### 6.1 Visual Studio Code (Recommended)

**Extensions to install:**

```powershell
# Rust
code --install-extension rust-lang.rust-analyzer

# Slint
code --install-extension slint.slint

# C++
code --install-extension ms-vscode.cpptools
code --install-extension ms-vscode.cmake-tools

# Docker
code --install-extension ms-azuretools.vscode-docker

# TOML (for Cargo.toml)
code --install-extension tamasfe.even-better-toml
```

**Workspace settings (.vscode/settings.json):**

```json
{
    "rust-analyzer.cargo.features": "all",
    "rust-analyzer.checkOnSave.command": "clippy",
    "editor.formatOnSave": true,
    "[rust]": {
        "editor.defaultFormatter": "rust-lang.rust-analyzer"
    },
    "cmake.configureOnOpen": true,
    "cmake.buildDirectory": "${workspaceFolder}/libmello/build",
    "slint.preview.style": "fluent-dark"
}
```

### 6.2 Visual Studio 2022 (for C++)

1. Open `libmello/build/mello.sln`
2. Set configuration to Release x64
3. Set startup project to `mello`

### 6.3 CLion (Alternative for C++)

1. Open `libmello/CMakeLists.txt` as project
2. Configure CMake toolchain to use Visual Studio

---

## 7. Running Everything

### 7.1 Start Order

```powershell
# Terminal 1: Backend
cd backend
docker compose up

# Terminal 2: Build and run client
cd client
cargo run
```

### 7.2 Development Workflow

```powershell
# Watch mode for Rust (auto-recompile)
cargo install cargo-watch
cargo watch -x run

# Or for faster iteration:
cargo watch -x check  # Only type-check, don't run
```

### 7.3 Testing Voice/Video Locally

For testing P2P between two instances:

```powershell
# Terminal 1: First client
cargo run

# Terminal 2: Second client (different port)
cargo run -- --port 8081
```

### 7.4 Full Stack Test

1. Start backend: `docker compose up -d`
2. Open Nakama console: http://localhost:7351
3. Start first client: `cargo run`
4. Login with test account
5. Create a crew
6. Start second client
7. Join the same crew
8. Test voice/streaming

---

## 8. Troubleshooting

### 8.1 Docker Issues

**"Cannot connect to Docker daemon"**
```powershell
# Ensure Docker Desktop is running
# Check WSL status
wsl --status
```

**"Port already in use"**
```powershell
# Find process using port
netstat -ano | findstr :7350

# Kill process
taskkill /PID <pid> /F
```

### 8.2 Rust Issues

**"linker not found"**
```powershell
# Ensure Visual Studio C++ tools are installed
# Run from "Developer PowerShell for VS 2022"
```

**"LIBCLANG_PATH not set"**
```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\lib"
```

### 8.3 CMake Issues

**"Could not find vcpkg"**
```powershell
# Ensure VCPKG_ROOT is set
$env:VCPKG_ROOT = "C:\vcpkg"

# Re-run cmake with toolchain file
cmake .. -DCMAKE_TOOLCHAIN_FILE="C:\vcpkg\scripts\buildsystems\vcpkg.cmake"
```

### 8.4 Nakama Issues

**"Connection refused"**
```powershell
# Check if Nakama is running
docker compose ps

# Check Nakama logs
docker compose logs nakama

# Restart Nakama
docker compose restart nakama
```

**"Database migration failed"**
```powershell
# Reset database
docker compose down -v
docker compose up -d
```

### 8.5 Audio/Video Issues

**"No audio devices found"**
- Check Windows audio settings
- Ensure microphone permissions are granted

**"NVENC initialization failed"**
- Ensure NVIDIA drivers are up to date
- Check GPU supports NVENC (GTX 600+)

---

## 9. Development Workflow

### 9.1 Branch Strategy

```
main              # Stable, release-ready
├── develop       # Integration branch
├── feature/*     # New features
├── fix/*         # Bug fixes
└── release/*     # Release preparation
```

### 9.2 Commit Convention

```
feat: add voice mute toggle
fix: resolve audio crackling on high CPU
refactor: simplify peer connection logic
docs: update getting started guide
test: add voice mesh unit tests
```

### 9.3 Pre-Commit Checklist

```powershell
# Format code
cargo fmt

# Lint
cargo clippy

# Run tests
cargo test

# For C++
cd libmello/build
cmake --build . --target format  # If configured
ctest
```

### 9.4 Debugging

**Rust/Client:**
```powershell
# Enable debug logging
$env:RUST_LOG = "debug"
cargo run

# Or specific module:
$env:RUST_LOG = "mello_core=debug"
```

**C++/libmello:**
- Use Visual Studio debugger
- Set breakpoints in source files
- Attach to running process if needed

**Network:**
```powershell
# Monitor WebSocket traffic
# Use browser DevTools or Wireshark

# Nakama debug logs
docker compose logs -f nakama
```

### 9.5 Performance Profiling

**Rust:**
```powershell
cargo install flamegraph
cargo flamegraph --bin mello-client
```

**C++ (Visual Studio):**
- Debug → Performance Profiler
- Select CPU Usage / GPU Usage

---

## 10. Next Steps

Once your environment is set up:

1. **Read the specs:**
   - [00-ARCHITECTURE.md](./00-ARCHITECTURE.md) - Overall architecture
   - [01-CLIENT.md](./01-CLIENT.md) - UI implementation
   - [02-MELLO-CORE.md](./02-MELLO-CORE.md) - Core logic
   - [03-LIBMELLO.md](./03-LIBMELLO.md) - Low-level C++
   - [04-BACKEND.md](./04-BACKEND.md) - Nakama backend

2. **Start with the UI shell:**
   - Build static Slint UI
   - No backend connection yet
   - Get layout and styling right

3. **Add voice P2P:**
   - Two-user voice connection
   - Test RNNoise + VAD

4. **Integrate with Nakama:**
   - Authentication
   - Presence
   - Chat

5. **Build in public:**
   - Tweet progress
   - Gather feedback
   - Iterate

---

## Appendix: Quick Reference

### Useful Commands

```powershell
# Backend
docker compose up -d          # Start
docker compose down           # Stop
docker compose logs -f        # View logs
docker compose restart        # Restart

# Rust
cargo build                   # Build debug
cargo build --release         # Build release
cargo run                     # Run
cargo test                    # Run tests
cargo clippy                  # Lint
cargo fmt                     # Format

# C++ (from libmello/build)
cmake --build . --config Release
ctest -C Release

# Git
git status
git pull --rebase
git push origin feature/xyz
```

### Key URLs

| Service | URL |
|---------|-----|
| Nakama Console | http://localhost:7351 |
| Nakama API | http://localhost:7350 |
| Nakama gRPC | http://localhost:7349 |
| PostgreSQL | localhost:5432 |

### Environment Variables

```powershell
# Required
$env:VCPKG_ROOT = "C:\vcpkg"
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\lib"

# Optional
$env:RUST_LOG = "debug"
$env:NVENC_SDK_PATH = "C:\Program Files\NVIDIA GPU Computing Toolkit\VideoCodecSDK"
```

---

*Happy coding! 🚀*
