# MELLO Getting Started Guide

> **Purpose:** Set up your development environment for Mello  
> **Time:** ~30-60 minutes  
> **OS:** Windows 10/11 (primary), macOS (client only)

---

## Quick Start

```bash
# 1. Clone
git clone https://github.com/mello-app/mello.git
cd mello

# 2. Start backend
cd backend && docker compose up -d

# 3. Run client (in another terminal)
cd client && cargo run
```

---

## 1. Prerequisites

| Tool | Version | Purpose | Windows | macOS |
|------|---------|---------|---------|-------|
| Git | Latest | Version control | `winget install Git.Git` | `brew install git` |
| Docker Desktop | Latest | Backend services | Docker Desktop | Docker Desktop |
| Rust | 1.75+ | Client + mello-core | `winget install Rustlang.Rustup` | `brew install rustup-init && rustup-init` |
| Visual Studio 2022 | Latest | C++ compiler, Windows SDK | VS Installer (Desktop C++ workload) | N/A |
| Xcode CLT | Latest | C++ compiler (macOS) | N/A | `xcode-select --install` |
| CMake | 3.20+ | C++ build system | `winget install Kitware.CMake` | `brew install cmake` |
| LLVM/Clang | Latest | bindgen (FFI generation) | `winget install LLVM.LLVM` | `brew install llvm` |

### Hardware

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| RAM | 8GB | 16GB |
| Disk | 20GB free | 50GB free |
| GPU | Any | NVIDIA GTX 1060+ (for NVENC streaming) |

---

## 2. Backend Setup

```bash
cd backend

# Copy environment template
cp .env.example .env
# Edit .env if needed (defaults work for local dev)

# Start Nakama + PostgreSQL
docker compose up -d

# Verify: open http://localhost:7351 (admin / mello_admin_dev)
```

### Common backend commands

```bash
docker compose logs -f nakama     # Stream Nakama logs
docker compose restart nakama     # Restart after module changes
docker compose down -v            # Full reset (wipes DB)
docker compose exec postgres psql -U nakama -d nakama  # SQL shell
```

Go modules in `backend/nakama/data/modules/` are hot-loaded by Nakama on restart.

---

## 3. Client Setup

```bash
cd client
cargo run            # Debug build + run
cargo build --release  # Release build
```

### Slint UI development

Install the Slint VS Code extension (`slint.slint`) for `.slint` file support and live preview. The Slint LSP is also available via `cargo install slint-lsp`.

---

## 4. libmello Setup (C++)

libmello builds natively on Windows. macOS support is partial (no DXGI/WASAPI — those are Windows-only).

### Windows

```powershell
cd libmello
mkdir build && cd build

cmake .. -G "Visual Studio 17 2022" -A x64 `
    -DCMAKE_TOOLCHAIN_FILE="$env:VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake"

cmake --build . --config Release
ctest -C Release --output-on-failure
```

Dependencies (via vcpkg or git submodules in `third_party/`):
- opus, rnnoise, libdatachannel, onnxruntime (Silero VAD)
- NVIDIA Video Codec SDK (for NVENC), AMD AMF SDK (optional)

### macOS

```bash
cd libmello && mkdir build && cd build
cmake .. && cmake --build .
```

Only transport and Opus codec build on macOS. Audio capture/playback and video capture/encode are Windows-only.

### FFI bindings

`mello-sys` generates Rust bindings from `libmello/include/mello.h` via bindgen. It builds automatically as a Cargo dependency. Tests in `mello-sys/tests/` require audio hardware and are excluded from `cargo test --workspace` by default.

---

## 5. Environment Variables

| Variable | Required | Platform | Purpose |
|----------|----------|----------|---------|
| `VCPKG_ROOT` | For C++ | Windows | vcpkg toolchain path |
| `LIBCLANG_PATH` | For bindgen | Both | LLVM lib path |
| `RUST_LOG` | No | Both | Log level (`debug`, `mello_core=debug`) |
| `NVENC_SDK_PATH` | For NVENC | Windows | NVIDIA Video Codec SDK path |

macOS LLVM path: `export LIBCLANG_PATH="$(brew --prefix llvm)/lib"`

---

## 6. Running the Full Stack

1. Start backend: `cd backend && docker compose up -d`
2. Verify Nakama console: http://localhost:7351
3. Run client: `cd client && cargo run`
4. The client starts in onboarding mode (no persisted session)
5. To test multi-user: run a second client instance

### Debugging

```bash
# Verbose Rust logging
RUST_LOG=debug cargo run

# Module-specific
RUST_LOG=mello_core=debug,mello_core::nakama=trace cargo run

# Nakama server logs
docker compose logs -f nakama
```

---

## 7. Development Workflow

### Pre-commit checklist

```bash
# Rust
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace

# C++ (from libmello/build)
cmake --build . && ctest --output-on-failure
```

### Commit convention

```
feat(voice): add VAD threshold config
fix(stream): resolve frame drop under high loss
refactor(nakama): simplify session restore
test(crew): add invite code round-trip test
docs: update backend RPC list
chore: bump slint to 1.9
```

### Branch strategy

- `main` — stable, never push directly
- `feature/*`, `fix/*` — short-lived branches, PR into main

---

## 8. Troubleshooting

| Problem | Fix |
|---------|-----|
| "Cannot connect to Docker daemon" | Start Docker Desktop, check WSL status (`wsl --status`) |
| "Port 7350 already in use" | `netstat -ano \| findstr :7350` then kill PID, or `lsof -i :7350` on macOS |
| "linker not found" (Rust) | Install VS 2022 C++ workload, or run from Developer PowerShell |
| `LIBCLANG_PATH` not set | Set to LLVM lib dir (see env vars above) |
| "Database migration failed" | `docker compose down -v && docker compose up -d` |
| "No audio devices found" | Check OS audio settings, grant mic permissions |
| "NVENC initialization failed" | Update NVIDIA drivers, verify GPU supports NVENC (GTX 600+) |
| `mello-sys` tests fail | These need audio hardware — they're excluded from workspace tests |

---

## 9. Key URLs

| Service | URL |
|---------|-----|
| Nakama Console | http://localhost:7351 |
| Nakama REST API | http://localhost:7350 |
| PostgreSQL | localhost:5432 |

---

*For architecture details, start with [00-ARCHITECTURE.md](./00-ARCHITECTURE.md).*
