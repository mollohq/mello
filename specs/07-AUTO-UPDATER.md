# MELLO Auto-Updater Specification

> **Component:** Auto-Update System  
> **Version:** 0.4
> **Status:** Implemented
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

Mello includes a built-in auto-updater powered by [Velopack](https://velopack.io). Velopack handles packaging, code signing, delta updates, and the full install/update lifecycle on Windows and macOS. Updates are distributed via GitHub Releases.

The current client checks for updates during startup. If an update is available, Mello shows a small Slint force-update window before the main app is created, downloads the update immediately, applies it, and restarts into the new version.

### Goals

- **Required at startup:** New versions are applied before the main app starts
- **Visible:** Show a focused startup dialog with version, progress, and failure state
- **Reliable:** Atomic updates with automatic rollback
- **Transparent:** Show download/apply progress
- **Signed:** All binaries are code-signed (Azure Trusted Signing on Windows, Apple Developer ID + notarization on macOS)
- **Efficient:** Delta updates minimize download size between versions

---

## 2. Update Flow

```text
App start
  -> Updater::run_lifecycle_hooks()
     -> VelopackApp::build().run()
        handles install/update/uninstall lifecycle hooks and may exit
  -> init logging / panic hook
  -> enforce single-instance
  -> Updater::new()
     source = MELLO_UPDATE_URL override, or GitHub Releases
     channel = MELLO_UPDATE_CHANNEL override, or Velopack package channel
  -> check_for_updates()
     no update: continue normal app startup
     update available: create ForceUpdateWindow before MainWindow
  -> startup_update::run_gate()
     shows a centered borderless progress dialog
     calls update_and_restart() immediately
     maps Velopack progress/errors to dialog state
  -> download_updates()
  -> apply_updates_and_restart()
     Velopack applies update atomically and restarts the app
```

The settings-panel updater still uses the same `Updater` event stream, but the primary path is the startup gate. The main app is not created until the check reports no update, the user explicitly continues after a soft failure, or Velopack restarts into the updated version.

**Key difference from a hand-rolled updater:** Velopack handles delta computation, checksum verification, atomic binary replacement, and restart internally. The client code calls `check_for_updates()`, `download_updates()`, and `apply_updates_and_restart()`.

---

## 3. GitHub Releases Structure

Velopack generates platform-specific artifacts that are uploaded to each GitHub Release.

```
mollohq/mello
└── releases
    └── v0.2.0
        ├── m3llo-win-x64-stable-Setup.exe      (Windows installer, signed via ATS)
        ├── m3llo-0.2.0-win-x64-stable-full.nupkg
        ├── m3llo-0.2.0-win-x64-stable-delta.nupkg
        ├── releases.win-x64-stable.json         (Windows release index — Velopack reads this)
        │
        ├── m3llo-osx-arm64-stable-Setup.pkg    (macOS installer, signed + notarized)
        ├── m3llo-0.2.0-osx-arm64-stable-full.nupkg
        ├── m3llo-0.2.0-osx-arm64-stable-delta.nupkg
        └── releases.osx-arm64-stable.json       (macOS release index — Velopack reads this)
```

### How Velopack uses the release index

The `releases.{channel}.json` file is the source of truth for updates. `UpdateManager` fetches this file from GitHub Releases and compares it against the installed version. It contains:

- Current latest version number
- List of available full and delta packages with SHA checksums
- Download URLs for each package

Velopack generates and updates this file automatically during `vpk pack`.

---

## 4. Implementation

### 4.1 Dependencies

```toml
# client/Cargo.toml

[dependencies]
velopack = "0.0.1298"       # Velopack Rust SDK
semver = "1"
tokio = { workspace = true }
```

The `self_update`, `sha2`, `hex`, `futures-util`, and `reqwest` (for update purposes) dependencies from the previous spec are **removed** — Velopack handles all of this internally.

### 4.2 Startup Hook

`Updater::run_lifecycle_hooks()` **must** be the very first call in `main()`, before any other initialization. It calls `VelopackApp::build().run()`, which handles install, uninstall, and update lifecycle hooks.

```rust
// client/src/main.rs

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ──── Velopack lifecycle hook — MUST be first ────
    Updater::run_lifecycle_hooks();
    // ─────────────────────────────────────────────────

    let log_dir = init_logging();

    // ... rest of startup (Slint, updater, etc.)
}
```

If the process was spawned by Velopack for a lifecycle event (install, uninstall, update apply), `run()` performs the hook and exits. Otherwise it returns immediately and normal startup continues.

### 4.3 Core Types

```rust
// client/src/updater/mod.rs

#[derive(Debug, Clone)]
pub enum UpdateEvent {
    CheckStarted,
    CheckComplete {
        update_available: bool,
        version: Option<String>,
        download_size: Option<u64>,
    },
    DownloadStarted {
        total_bytes: u64,
    },
    DownloadProgress {
        progress: f32,
        downloaded_bytes: u64,
        total_bytes: u64,
    },
    DownloadComplete,
    ApplyStarted,
    Error(String),
}
```

### 4.4 Updater Implementation

`client/src/updater/updater.rs` wraps Velopack's `UpdateManager` and keeps the latest `UpdateInfo` cached after `check_for_updates()`. It uses a standard `mpsc::Sender<UpdateEvent>` so both the startup gate and settings panel can consume update state without binding the updater to Slint.

Important implementation points:

- `Updater::run_lifecycle_hooks()` calls `VelopackApp::build().run()` and must remain the first call in `main()`.
- `Updater::new()` uses `MELLO_UPDATE_URL` as an HTTP or local file-source override. Without it, the source is GitHub Releases: `https://github.com/mollohq/mello/releases/latest/download/`.
- `MELLO_UPDATE_CHANNEL` is an optional local/dev override that maps to Velopack `UpdateOptions::ExplicitChannel`. CI does not set it; production releases use the channel embedded in the installed package.
- `check_for_updates()` emits `CheckStarted`, `CheckComplete`, and caches the `UpdateInfo`.
- `update_and_restart()` runs download/apply on a background thread, emits progress events, then calls `apply_updates_and_restart()`.
- `MELLO_UPDATE_APPLY_DELAY_MS` is a test-only delay before apply, used to keep the startup dialog visible during local scenarios.

### 4.5 Integration with App Startup

Current startup order in `client/src/main.rs`:

1. `Updater::run_lifecycle_hooks()` is called first in `main()`.
2. `run_app()` enforces single-instance and creates the updater.
3. `check_for_updates()` runs synchronously before `MainWindow::new()`.
4. If an update is available:
   - `startup_update::configure_slint_platform()` initializes the pre-main Slint platform on macOS.
   - `startup_update::run_gate()` creates `ForceUpdateWindow`, starts the download immediately, and runs a temporary Slint event loop.
   - success path restarts the app via Velopack; soft failure lets the user continue into the current version.
5. If no update is available, startup continues normally and the main window/poll loop reuse a fresh update-event channel.

---

## 5. UI Components

### 5.1 Startup Force-Update Dialog

The startup dialog is `ForceUpdateWindow` from `client/ui/panels/force_update_dialog.slint`, exported through `client/ui/main.slint` and driven by `client/src/updater/startup_update.rs`.

Behavior:

- Borderless fixed-size window: `360px` wide, `188px` tall during download, `244px` on failure.
- Shows current version, target version, stage, percentage, byte count, and failure details.
- Close button is intercepted with `CloseRequestResponse::KeepWindowShown`.
- Retry starts `update_and_restart()` again.
- Non-hard failures show a secondary action to start the current version.
- macOS uses the winit Slint backend with the default menu bar disabled for the pre-main window.
- Windows and macOS center the dialog on the primary screen before showing it.

### 5.2 Historical Banner UI

Earlier revisions planned an in-app update banner. That design is superseded by the startup gate, but the settings-panel updater may still reuse the same `Updater` events for manual update affordances.

```slint
// client/ui/components/update_banner.slint

import { Theme } from "../theme.slint";

export component UpdateBanner inherits Rectangle {
    in property <bool> visible: false;
    in property <string> version: "";
    in property <string> release-notes: "";
    in property <float> download-progress: -1.0;  // -1 = not downloading
    in property <bool> ready-to-install: false;
    
    callback update-clicked();
    callback restart-clicked();
    callback dismiss-clicked();
    
    height: visible ? 56px : 0px;
    background: #2563eb;
    clip: true;
    
    animate height { duration: 200ms; easing: ease-out; }
    
    if visible: HorizontalLayout {
        padding-left: Theme.spacing-lg;
        padding-right: Theme.spacing-lg;
        spacing: Theme.spacing-md;
        alignment: center;
        
        // Icon
        Text {
            text: "🎉";
            font-size: 20px;
            vertical-alignment: center;
        }
        
        // Message
        Text {
            text: ready-to-install 
                ? "Update ready! Restart to complete."
                : download-progress >= 0 
                    ? "Downloading v" + version + "..."
                    : "Mello v" + version + " is available!";
            color: white;
            font-size: Theme.font-md;
            vertical-alignment: center;
        }
        
        // Progress bar (when downloading)
        if download-progress >= 0 && !ready-to-install: Rectangle {
            width: 150px;
            height: 8px;
            background: #ffffff44;
            border-radius: 4px;
            
            Rectangle {
                width: parent.width * clamp(download-progress, 0, 1);
                height: parent.height;
                background: white;
                border-radius: 4px;
                
                animate width { duration: 100ms; }
            }
        }
        
        Rectangle { horizontal-stretch: 1; }
        
        // Action button
        Rectangle {
            width: ready-to-install ? 100px : 120px;
            height: 36px;
            background: white;
            border-radius: 6px;
            
            Text {
                text: ready-to-install ? "Restart" : "Update Now";
                color: #2563eb;
                font-size: Theme.font-md;
                font-weight: 600;
                horizontal-alignment: center;
                vertical-alignment: center;
            }
            
            TouchArea {
                clicked => {
                    if ready-to-install {
                        restart-clicked();
                    } else {
                        update-clicked();
                    }
                }
            }
        }
        
        // Dismiss button
        Rectangle {
            width: 36px;
            height: 36px;
            border-radius: 18px;
            background: transparent;
            
            Text {
                text: "✕";
                color: white;
                font-size: Theme.font-lg;
                horizontal-alignment: center;
                vertical-alignment: center;
            }
            
            TouchArea {
                clicked => { dismiss-clicked(); }
            }
        }
    }
}
```

### 5.2 Update Dialog (Optional, for release notes)

```slint
// client/ui/dialogs/update_dialog.slint

import { Theme } from "../theme.slint";

export component UpdateDialog inherits Rectangle {
    in property <bool> visible: false;
    in property <string> current-version: "";
    in property <string> new-version: "";
    in property <string> release-notes: "";
    
    callback update-clicked();
    callback later-clicked();
    
    if visible: Rectangle {
        width: 100%;
        height: 100%;
        background: #00000088;
        
        // Dialog
        Rectangle {
            x: (parent.width - self.width) / 2;
            y: (parent.height - self.height) / 2;
            width: 480px;
            height: 400px;
            background: Theme.bg-primary;
            border-radius: Theme.radius-lg;
            drop-shadow-blur: 20px;
            drop-shadow-color: #00000044;
            
            VerticalLayout {
                padding: Theme.spacing-lg;
                spacing: Theme.spacing-md;
                
                // Header
                Text {
                    text: "Update Available 🎉";
                    font-size: Theme.font-xl;
                    font-weight: 700;
                }
                
                Text {
                    text: "v" + current-version + " → v" + new-version;
                    font-size: Theme.font-md;
                    color: Theme.text-secondary;
                }
                
                // Release notes
                Rectangle {
                    vertical-stretch: 1;
                    background: Theme.bg-secondary;
                    border-radius: Theme.radius-md;
                    
                    Flickable {
                        padding: Theme.spacing-md;
                        
                        Text {
                            text: release-notes;
                            font-size: Theme.font-sm;
                            color: Theme.text-primary;
                            wrap: word-wrap;
                        }
                    }
                }
                
                // Buttons
                HorizontalLayout {
                    spacing: Theme.spacing-md;
                    alignment: end;
                    
                    Rectangle {
                        width: 100px;
                        height: 40px;
                        background: Theme.bg-secondary;
                        border-radius: Theme.radius-md;
                        
                        Text {
                            text: "Later";
                            color: Theme.text-secondary;
                            horizontal-alignment: center;
                            vertical-alignment: center;
                        }
                        
                        TouchArea {
                            clicked => { later-clicked(); }
                        }
                    }
                    
                    Rectangle {
                        width: 140px;
                        height: 40px;
                        background: #2563eb;
                        border-radius: Theme.radius-md;
                        
                        Text {
                            text: "Update Now";
                            color: white;
                            font-weight: 600;
                            horizontal-alignment: center;
                            vertical-alignment: center;
                        }
                        
                        TouchArea {
                            clicked => { update-clicked(); }
                        }
                    }
                }
            }
        }
    }
}
```

The UI components are wired to the Velopack-backed `Updater` events rather than the old custom updater. The callbacks map as follows:

| UI callback | Updater method |
|---|---|
| `update-clicked()` | `updater.download()` |
| `restart-clicked()` | `updater.apply_and_restart()` |
| `dismiss-clicked()` | Set `UpdateStatus::Idle` (hide banner until next startup) |

---

## 6. GitHub Actions: Build & Release

The current release workflow is `.github/workflows/release.yml`. It has separate Windows and macOS jobs, then a publish job that uploads both platforms' Velopack artifacts to the GitHub Release.

Both platform jobs stamp the workspace version, refresh the lockfile offline, build with production features, run tests and clippy, assemble `dist/`, download the previous release artifacts for delta generation, and run `vpk pack`.

### 6.1 Windows Workflow

```yaml
# .github/workflows/release.yml

name: Windows Release

on:
  release:
    types: [published]

  workflow_dispatch:
    inputs:
      reason:
        description: "Why are you triggering a manual build?"
        required: true
        default: "I need a binary package for testing!"

permissions:
  contents: write

env:
  RELEASE_CHANNEL: "win-x64-stable"

jobs:
  build-windows:
    runs-on: windows-latest

    steps:
      - name: Checkout repository
        uses: actions/checkout@v4
        with:
          fetch-depth: 0
          submodules: recursive
          token: ${{ secrets.GH_PAT }}

      - name: Setup Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: x86_64-pc-windows-msvc

      - name: Rust cache
        uses: Swatinem/rust-cache@v2

      - name: Set version from release
        if: github.event_name == 'release'
        run: |
          $version = "${{ github.event.release.tag_name }}" -replace '^v',''
          echo "RELEASE_VERSION=$version" >> $env:GITHUB_ENV

      - name: Set dev version
        if: github.event_name == 'workflow_dispatch'
        run: |
          echo "RELEASE_VERSION=0.0.${{ github.run_number }}" >> $env:GITHUB_ENV

      # Build libmello (cmake)
      - name: Build libmello
        shell: bash
        run: |
          cd libmello
          cmake -B build -DCMAKE_BUILD_TYPE=Release
          cmake --build build --config Release

      # Build Mello client
      - name: Build client
        run: cargo build --release --target x86_64-pc-windows-msvc

      # Assemble dist folder for vpk pack
      - name: Assemble dist
        shell: pwsh
        run: |
          mkdir dist
          cp target/x86_64-pc-windows-msvc/release/mello.exe dist/
          # ONNX Runtime dynamic libraries
          cp libmello/third_party/onnxruntime/onnxruntime-win-x64-*/lib/onnxruntime.dll dist/
          cp libmello/third_party/onnxruntime/onnxruntime-win-x64-*/lib/onnxruntime_providers_shared.dll dist/
          # Remove any .lib files that snuck in
          del dist/*.lib -ErrorAction SilentlyContinue

      # Azure login for Trusted Signing
      - name: Login to Azure
        uses: azure/login@v2
        with:
          creds: ${{ secrets.AZURE_CREDENTIALS }}

      - name: Verify Azure Login
        run: az account show --output table

      # Velopack: download previous release for delta generation, then pack
      - name: Velopack Package
        shell: cmd
        run: |
          dotnet tool install -g vpk
          mkdir vpk-out
          vpk download github --repoUrl https://github.com/mollohq/mello --outputDir vpk-out --channel %RELEASE_CHANNEL% --token ${{ secrets.GH_PAT }}
          vpk pack ^
            --packId m3llo ^
            --packVersion %RELEASE_VERSION% ^
            --packDir dist ^
            --mainExe mello.exe ^
            --packTitle Mello ^
            --channel %RELEASE_CHANNEL% ^
            --outputDir vpk-out ^
            --icon client/assets/icons/mello.ico ^
            --azureTrustedSignFile client/windows/signing-metadata.json

      # Upload Velopack artifacts to the GitHub Release
      - name: Upload to GitHub Release
        if: github.event_name == 'release'
        shell: pwsh
        run: |
          $files = Get-ChildItem vpk-out -File
          foreach ($f in $files) {
            gh release upload "${{ github.event.release.tag_name }}" $f.FullName --clobber
          }
        env:
          GH_TOKEN: ${{ secrets.GH_PAT }}

      # Also upload as build artifacts for manual/dispatch runs
      - name: Upload build artifacts
        uses: actions/upload-artifact@v4
        with:
          name: mello-windows-x64
          path: vpk-out/*
```

### 6.2 macOS Workflow

```yaml
# .github/workflows/release.yml

name: macOS Release

on:
  release:
    types: [published]

  workflow_dispatch:
    inputs:
      reason:
        description: "Why are you triggering a manual build?"
        required: true
        default: "I need a binary package for testing!"

permissions:
  contents: write

env:
  RELEASE_CHANNEL: "osx-arm64-stable"

jobs:
  build-macos:
    runs-on: self-hosted  # macOS self-hosted runner with Apple certs

    steps:
      - name: Checkout repository
        uses: actions/checkout@v4
        with:
          fetch-depth: 0
          submodules: recursive
          token: ${{ secrets.GH_PAT }}

      - name: Setup Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-apple-darwin

      - name: Rust cache
        uses: Swatinem/rust-cache@v2

      - name: Set version from release
        if: github.event_name == 'release'
        run: |
          version="${{ github.event.release.tag_name }}"
          version="${version#v}"
          echo "RELEASE_VERSION=$version" >> $GITHUB_ENV

      - name: Set dev version
        if: github.event_name == 'workflow_dispatch'
        run: |
          echo "RELEASE_VERSION=0.0.${{ github.run_number }}" >> $GITHUB_ENV

      # Build libmello (cmake)
      - name: Build libmello
        run: |
          cd libmello
          cmake -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_ARCHITECTURES=arm64
          cmake --build build --config Release

      # Build Mello client
      - name: Build client
        run: cargo build --release --target aarch64-apple-darwin

      # Assemble dist folder
      - name: Assemble dist
        run: |
          mkdir -p dist
          cp target/aarch64-apple-darwin/release/mello dist/
          # ONNX Runtime dynamic library
          cp libmello/third_party/onnxruntime/onnxruntime-osx-arm64-*/lib/libonnxruntime*.dylib dist/

      # Import Apple signing certificates
      - name: Import Apple certificates
        uses: apple-actions/import-codesign-certs@v2
        with:
          p12-file-base64: ${{ secrets.MACOS_BUILD_AND_INSTALLER_CERTIFICATES }}
          p12-password: ${{ secrets.P12_PASSWORD }}

      # Store notarytool credentials in a temporary keychain profile
      - name: Store notarytool credentials
        run: |
          xcrun notarytool store-credentials "AC_PASSWORD" \
            --apple-id "${{ secrets.APPLE_ID }}" \
            --team-id "${{ secrets.APPLE_TEAM }}" \
            --password "${{ secrets.APPLE_PASSWORD }}"

      # Manually codesign ONNX Runtime dylib before Velopack packaging
      # (Velopack signs the main binary, but external dylibs must be pre-signed)
      - name: Codesign ONNX Runtime dylib
        run: |
          for dylib in dist/libonnxruntime*.dylib; do
            codesign --force --options runtime --timestamp \
              --sign "Developer ID Application: ${{ secrets.APPLE_TEAM_NAME }}" \
              "$dylib"
          done

      # Velopack: download previous release for delta, then pack + sign + notarize
      - name: Velopack Package
        run: |
          dotnet tool install -g vpk
          mkdir -p vpk-out
          vpk download github --repoUrl https://github.com/mollohq/mello --outputDir vpk-out --channel $RELEASE_CHANNEL --token ${{ secrets.GH_PAT }}
          vpk pack \
            --packId m3llo \
            --packVersion $RELEASE_VERSION \
            --packDir dist \
            --mainExe mello \
            --packTitle Mello \
            --channel $RELEASE_CHANNEL \
            --outputDir vpk-out \
            --icon client/assets/icons/mello.icns \
            --signAppIdentity "Developer ID Application: ${{ secrets.APPLE_TEAM_NAME }}" \
            --signInstallIdentity "Developer ID Installer: ${{ secrets.APPLE_TEAM_NAME }}" \
            --notaryProfile "${{ env.NOTARY_PROFILE_NAME }}" \
            --keychain "$KEYCHAIN_PATH" \
            --signEntitlements client/macos/release.entitlements \
            --plist client/macos/Info.plist

      # Upload Velopack artifacts to the GitHub Release
      - name: Upload to GitHub Release
        if: github.event_name == 'release'
        run: |
          for f in vpk-out/*; do
            gh release upload "${{ github.event.release.tag_name }}" "$f" --clobber
          done
        env:
          GH_TOKEN: ${{ secrets.GH_PAT }}

      # Also upload as build artifacts
      - name: Upload build artifacts
        uses: actions/upload-artifact@v4
        with:
          name: mello-macos-arm64
          path: vpk-out/*

      # Clean up keychain (always, even if build fails)
      - name: Cleanup keychain
        if: always()
        run: |
          security delete-keychain signing_temp.keychain || true
```

---

## 7. Code Signing Setup

### 7.1 Windows — Azure Trusted Signing (ATS)

Azure Trusted Signing provides EV-equivalent code signing with instant SmartScreen reputation (no more "Windows protected your PC" dialogs).

**Setup:**

1. Create an Azure Trusted Signing resource in the Azure Portal
2. Create a certificate profile (public trust, code signing)
3. Create a service principal with `Trusted Signing Certificate Profile Signer` role
4. Save the service principal credentials as the `AZURE_CREDENTIALS` GitHub secret

**Signing metadata file** (committed to the repo):

```json
// windows/signing-metadata.json
{
  "Endpoint": "https://eus.codesigning.azure.net/",
  "CodeSigningAccountName": "mollohq",
  "CertificateProfileName": "mollohq"
}
```

The `vpk pack --azureTrustedSignFile` flag tells Velopack to sign the installer, update packages, and the main executable using these credentials. Azure login must happen before `vpk pack`.

### 7.2 macOS — Apple Developer ID + Notarization

macOS requires two layers: code signing (Developer ID certificates) and notarization (Apple's automated security check).

**Certificates required:**

| Certificate | Used for |
|---|---|
| Developer ID Application | Signing the app binary, frameworks, dylibs |
| Developer ID Installer | Signing the `.pkg` installer |

Both are exported as a single `.p12` file and stored as a base64-encoded GitHub secret.

**Entitlements file** (committed to the repo):

```xml
<!-- client/macos/release.entitlements -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.cs.allow-jit</key>
    <true/>
    <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
    <true/>
    <key>com.apple.security.device.audio-input</key>
    <true/>
    <key>com.apple.security.network.client</key>
    <true/>
    <key>com.apple.security.network.server</key>
    <true/>
</dict>
</plist>
```

Entitlements grant the app permissions for:
- JIT and unsigned executable memory (needed by ONNX Runtime)
- Microphone access (core feature)
- Network client/server (WebRTC, API calls)

**External dylib pre-signing:** `libonnxruntime.dylib` must be codesigned with the Developer ID Application identity *before* `vpk pack` runs. Velopack signs the main executable and the `.pkg`, but external dynamic libraries need to be pre-signed manually. This is the same pattern used for Molly.

### 7.3 Required GitHub Secrets

| Secret | Platform | Description |
|---|---|---|
| `GH_PAT` | Both | GitHub Personal Access Token (for release uploads + Velopack delta download) |
| `AZURE_CREDENTIALS` | Windows | Azure service principal JSON for Trusted Signing |
| `MACOS_BUILD_AND_INSTALLER_CERTIFICATES` | macOS | Base64-encoded `.p12` containing both Developer ID Application and Installer certs |
| `P12_PASSWORD` | macOS | Password for the `.p12` file |
| `APPLE_ID` | macOS | Apple ID email for notarytool |
| `APPLE_TEAM` | macOS | Apple Developer Team ID |
| `APPLE_TEAM_NAME` | macOS | Full team name string (e.g. "Mollo HQ Ltd") used in signing identity |
| `APPLE_PASSWORD` | macOS | App-specific password for notarytool |

---

## 8. Configuration

There is no persisted updater settings object today. The updater follows the channel embedded in the installed Velopack package, with environment overrides for local testing and diagnostics:

| Variable | Purpose |
|---|---|
| `MELLO_UPDATE_URL` | Overrides the update source. HTTP URLs use `HttpSource`; any other value is treated as a local `FileSource`. |
| `MELLO_UPDATE_CHANNEL` | Optional Velopack explicit-channel override. Used by local dev scripts to force `osx-arm64-dev` or `win-x64-dev`; CI does not set this. |
| `MELLO_UPDATE_APPLY_DELAY_MS` | Optional delay between download completion and apply/restart, useful for seeing the startup dialog during tests. |

Velopack channels map to the `--channel` flag used during `vpk pack`:

| UpdateChannel | Velopack channel (Windows) | Velopack channel (macOS) |
|---|---|---|
| Stable | `win-x64-stable` | `osx-arm64-stable` |
| Beta | `win-x64-beta` | `osx-arm64-beta` |

The channel determines which `releases.{channel}.json` the `UpdateManager` reads from GitHub Releases.

### 8.1 Local Test Scripts

Windows:

```powershell
.\scripts\test-updater.ps1 -Action scenario
```

macOS:

```bash
./scripts/test-updater.sh --action scenario --old-version 0.3.0 --new-version 0.3.1
```

The macOS script:

- Builds and packages unsigned local Velopack releases on `osx-arm64-dev`.
- Installs the old version to the current user's app location.
- Packs a newer version into `vpk-out/`.
- Launches the installed app binary directly so `MELLO_UPDATE_URL`, `MELLO_UPDATE_CHANNEL`, and `MELLO_UPDATE_APPLY_DELAY_MS` are inherited.
- Backs up and restores `Cargo.toml`, `Cargo.lock`, and `client/macos/Info.plist` while stamping temporary versions.

---

## 9. Testing Checklist

- [ ] `VelopackApp::build().run()` is the first call in `main()`
- [ ] Update check works on startup before `MainWindow` is created
- [ ] Startup force-update dialog appears when update is available
- [ ] Download shows progress via Velopack callback
- [ ] Startup dialog starts download immediately
- [ ] `apply_updates_and_restart()` relaunches on the new version
- [ ] Delta updates work when previous release exists
- [ ] Full update works for first install or when delta is unavailable
- [ ] Soft failure allows starting the current version
- [ ] Handles offline gracefully (update check logs and continues)
- [ ] Updater init fails gracefully in dev mode (not installed via Velopack)
- [ ] **Windows:** Installer (`Mello-Setup.exe`) runs without SmartScreen warning
- [ ] **Windows:** Installed app has valid Authenticode signature (check with `signtool verify`)
- [ ] **macOS:** `.pkg` installer runs without Gatekeeper warning
- [ ] **macOS:** App binary passes `codesign --verify --deep --strict`
- [ ] **macOS:** App passes `spctl --assess --type install` (notarization check)
- [ ] **macOS:** `libonnxruntime.dylib` is correctly signed alongside the app
- [ ] **macOS local:** `scripts/test-updater.sh --action scenario` shows the centered startup update dialog and applies the newer local package

---

## 10. Security Considerations

| Concern | Mitigation |
|---------|------------|
| MITM attacks | HTTPS only for all GitHub API and download traffic |
| Binary tampering | Velopack verifies SHA checksums on all packages; code signing provides authenticity |
| Code signing bypass | Azure Trusted Signing (Windows) and Apple notarization (macOS) ensure OS-level trust |
| SmartScreen / Gatekeeper warnings | ATS provides instant reputation on Windows; notarization satisfies Gatekeeper on macOS |
| Privilege escalation | No admin required on Windows (per-user install); macOS uses standard `.pkg` flow |
| Rollback attacks | Velopack only applies updates to newer versions |
| Supply chain | GitHub Actions secrets are encrypted; signing keys never leave Azure (ATS) or the self-hosted runner keychain |

---

## 11. Rollback Strategy

Velopack keeps the previous full `.nupkg` in its local package cache. If an update causes issues:

1. **Automatic:** If the new version crashes on startup, Velopack can detect this and roll back to the previous version automatically (configurable via `VelopackApp::build()` options)
2. **Manual:** Users can download the previous version's installer from GitHub Releases
3. **Future:** Add a "Rollback to previous version" button in Settings that triggers Velopack's rollback API

---

## 12. Protocol Version Handshake (Client ↔ Server Compatibility)

During early development both client and backend change frequently. Self-hosters run their own Nakama server but receive client updates from GitHub Releases, so the two can drift apart. A lightweight **protocol version** integer prevents silent breakage.

### Design

| Constant | Lives in | Meaning |
|---|---|---|
| `PROTOCOL_VERSION` | Client (mello-core) **and** Server (Go) | The protocol revision this build speaks |
| `MIN_SERVER_PROTOCOL` | Client (mello-core) | Oldest server protocol the client can tolerate |
| `MinClientProtocol` | Server (Go) | Oldest client protocol the server can tolerate |

**When to bump `PROTOCOL_VERSION`:** any breaking change to the client ↔ server contract — new required RPCs, changed RPC payloads, removed endpoints, changed match/presence data formats. Additive, backwards-compatible changes (new optional fields) do **not** require a bump.

### Server side

The existing `health` RPC is extended:

```go
// backend/nakama/data/modules/main.go

const (
    ProtocolVersion    = 1
    MinClientProtocol  = 1
)

func HealthCheckRPC(...) (string, error) {
    ...
    return fmt.Sprintf(
        `{"status":"healthy","version":"0.3.0","protocol_version":%d,"min_client_protocol":%d}`,
        ProtocolVersion, MinClientProtocol,
    ), nil
}
```

### Client side

After successful auth the client calls the `health` RPC and compares versions:

```
client.PROTOCOL_VERSION < server.min_client_protocol  →  "Please update Mello"
server.protocol_version < client.MIN_SERVER_PROTOCOL   →  "Server needs updating"
```

Both cases emit a `ProtocolMismatch` event that the UI should render as a non-dismissable compatibility warning. The app remains functional for best-effort use but warns clearly.

### Constants (initial values)

```rust
// mello-core
pub const PROTOCOL_VERSION: u32 = 1;
pub const MIN_SERVER_PROTOCOL: u32 = 1;
```

```go
// backend
const ProtocolVersion = 1
const MinClientProtocol = 1
```

### Why not semver / API versioning?

Semver doesn't map cleanly to "can these two talk to each other." A single integer is trivial to reason about, costs zero bytes on the wire beyond the health response, and requires no compatibility matrices. Good enough for beta; revisit if/when we have a public API.

---

*This spec covers auto-updates, packaging, code signing, and client-server version compatibility. For backend hosting, see [08-BACKEND-HOSTING.md](./08-BACKEND-HOSTING.md). For native platform integration, see [12-NATIVE-PLATFORM.md](./12-NATIVE-PLATFORM.md).*
