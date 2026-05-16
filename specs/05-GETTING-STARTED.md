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

# Start Nakama + PostgreSQL + MinIO (local S3)
docker compose up -d

# Verify: open http://localhost:7351 (admin / mello_admin_dev)
# MinIO console: http://localhost:9001 (minioadmin / minioadmin)
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

**S3/R2 variables** (set automatically by docker-compose for local dev):

| Variable | Local Dev Value | Purpose |
|----------|----------------|---------|
| `S3_ENDPOINT` | `http://minio:9000` | S3 API (Docker-internal) |
| `S3_PRESIGN_ENDPOINT` | `http://localhost:9000` | Presigned URL base (client-reachable) |
| `S3_BUCKET` | `mello-clips` | Bucket name |
| `S3_ACCESS_KEY` | `minioadmin` | S3 credentials |
| `S3_SECRET_KEY` | `minioadmin` | S3 credentials |
| `S3_PUBLIC_URL` | `http://localhost:9000/mello-clips` | Public download base URL |

These are pre-configured in `docker-compose.yml`. No manual setup needed for local dev.

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
| MinIO Console | http://localhost:9001 (minioadmin / minioadmin) |
| MinIO S3 API | http://localhost:9000 |

---

## 10. Cloudflare R2 Setup (Production)

Clips upload to Cloudflare R2 in production. R2 is S3-compatible with zero egress fees.

### 10.1 Create the bucket

1. Go to **Cloudflare Dashboard → R2 Object Storage**
2. Create bucket: `mello-clips`
3. Region: **Automatic** (EEUR recommended for EU-first)

### 10.2 Enable public access

1. In the bucket settings, go to **Settings → Public Access**
2. Enable **R2.dev subdomain** (gives you a `https://<random>.r2.dev` URL) — or —
3. **Connect a custom domain:** `clips.m3llo.app` (recommended). Add a CNAME record pointing to the R2 bucket. Cloudflare handles TLS automatically.

The public URL is what `S3_PUBLIC_URL` points to. All clip downloads go through this — no signed URLs needed for reads.

### 10.3 Create API token

1. Go to **R2 → Manage R2 API Tokens → Create API Token**
2. Permissions: **Object Read & Write**
3. Scope: Bucket `mello-clips` only
4. Copy the **Access Key ID** and **Secret Access Key**

### 10.4 Set environment variables

In your hosting provider (Render, etc.), set:

```
S3_ENDPOINT=https://<account-id>.r2.cloudflarestorage.com
S3_BUCKET=mello-clips
S3_ACCESS_KEY=<access-key-id>
S3_SECRET_KEY=<secret-access-key>
S3_PUBLIC_URL=https://clips.m3llo.app
```

`S3_PRESIGN_ENDPOINT` is **not needed** in production — the R2 endpoint is directly reachable by clients (unlike Docker where MinIO has an internal hostname).

### 10.5 Retention lifecycle (future)

For free-tier 7-day clip expiry, create an R2 lifecycle rule:

1. Bucket → **Settings → Object lifecycle**
2. Add rule: Delete objects after **7 days**
3. Prefix filter: `crews/` (applies to all crew clips)

m3llo+ subscribers will need clips exempted from this rule (separate prefix or bucket). Not needed for beta.

### 10.6 CORS (if needed)

R2 respects CORS headers. If the desktop client needs to PUT directly:

```json
[
  {
    "AllowedOrigins": ["*"],
    "AllowedMethods": ["PUT"],
    "AllowedHeaders": ["Content-Type"],
    "MaxAgeSeconds": 3600
  }
]
```

Current client uses `reqwest` (not a browser), so CORS is typically not required. Add this if/when a web client ships.

### 10.7 Snapshots R2 bucket

Stream snapshots use a second R2 bucket. Same account, same API token (widen scope to include both buckets).

1. Create bucket: `mello-snapshots` (same region as `mello-clips`)
2. Enable public access with custom domain: `snapshots.m3llo.app`
3. Ensure the R2 API token covers both `mello-clips` and `mello-snapshots`

**Nakama env vars** (Render):

```
SNAPSHOTS_S3_BUCKET=mello-snapshots
SNAPSHOTS_S3_PUBLIC_URL=https://snapshots.m3llo.app
```

Reuses `S3_ENDPOINT`, `S3_ACCESS_KEY`, `S3_SECRET_KEY` from clips. Nakama only needs `ListObjectsV2` on this bucket.

**SFU env vars** (in `/etc/sfu-certs/sfu.env` on each SFU VM):

```
SFU_R2_ENDPOINT=https://<account-id>.r2.cloudflarestorage.com
SFU_R2_BUCKET=mello-snapshots
SFU_R2_ACCESS_KEY_ID=<access-key-id>
SFU_R2_SECRET_ACCESS_KEY=<secret-access-key>
SFU_R2_PUBLIC_DOMAIN=snapshots.m3llo.app
```

The SFU writes snapshot JPEGs; Nakama only lists them. See [STREAM-SNAPSHOTS-CLIENT.md](./ongoing/STREAM-SNAPSHOTS-CLIENT.md) and `mello-sfu/STREAM-SNAPSHOTS-SFU.md`.

---

*For architecture details, start with [00-ARCHITECTURE.md](./00-ARCHITECTURE.md).*
