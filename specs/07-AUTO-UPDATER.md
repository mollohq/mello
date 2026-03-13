# MELLO Auto-Updater Specification

> **Component:** Auto-Update System  
> **Version:** 0.3  
> **Status:** Planned  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

Mello includes a built-in auto-updater powered by [Velopack](https://velopack.io). Velopack handles packaging, code signing, delta updates, and the full install/update lifecycle on Windows and macOS. Updates are distributed via GitHub Releases.

### Goals

- **Seamless:** Check on startup, non-blocking UI
- **User-controlled:** User decides when to install
- **Reliable:** Atomic updates with automatic rollback
- **Transparent:** Show release notes and download progress
- **Signed:** All binaries are code-signed (Azure Trusted Signing on Windows, Apple Developer ID + notarization on macOS)
- **Efficient:** Delta updates minimize download size between versions

---

## 2. Update Flow

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         UPDATE FLOW                                     │
│                                                                         │
│   ┌─────────────┐                                                       │
│   │ App Start   │                                                       │
│   └──────┬──────┘                                                       │
│          │                                                              │
│          ▼                                                              │
│   ┌───────────────────────────┐                                         │
│   │ VelopackApp::build().run()│  ← MUST be first call in main()        │
│   │ (handles install/uninstall│    Velopack lifecycle hooks run here    │
│   │  hooks silently)          │                                         │
│   └──────┬────────────────────┘                                         │
│          │                                                              │
│          ▼                                                              │
│   ┌─────────────────────────┐                                           │
│   │ UpdateManager::new()    │◀── Source: GitHub Releases                │
│   │ check_for_updates()     │    (reads releases.{channel}.json)       │
│   │ (background thread)     │                                           │
│   └──────┬──────────────────┘                                           │
│          │                                                              │
│          ▼                                                              │
│   ┌─────────────────────────┐     ┌────────────────────────────────┐    │
│   │ Update available?       │────▶│ Yes → show banner in UI        │    │
│   │ (Velopack compares      │     │ No  → up to date, done         │    │
│   │  against release index) │     └────────────────────────────────┘    │
│   └──────┬──────────────────┘                                           │
│          │                                                              │
│          ▼ (user clicks "Update")                                       │
│   ┌─────────────────────────┐                                           │
│   │ download_updates()      │──── Velopack downloads delta or full     │
│   │ (background)            │     nupkg, shows progress via callback   │
│   └──────┬──────────────────┘                                           │
│          │                                                              │
│          ▼                                                              │
│   ┌─────────────────────────┐                                           │
│   │ apply_updates_and_      │                                           │
│   │ restart()               │──── Velopack applies update atomically   │
│   │                         │     and restarts the app                 │
│   └─────────────────────────┘                                           │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

**Key difference from a hand-rolled updater:** Velopack handles delta computation, checksum verification, atomic binary replacement, and restart internally. The client code only calls three methods: `check_for_updates()`, `download_updates()`, `apply_updates_and_restart()`.

---

## 3. GitHub Releases Structure

Velopack generates platform-specific artifacts that are uploaded to each GitHub Release.

```
mollohq/mello
└── releases
    └── v0.2.0
        ├── Mello-Setup.exe                     (Windows installer, signed via ATS)
        ├── Mello-0.2.0-win-x64-full.nupkg      (Windows full update package)
        ├── Mello-0.2.0-win-x64-delta.nupkg     (Windows delta update, if prev release exists)
        ├── releases.win-x64.json                (Windows release index — Velopack reads this)
        │
        ├── Mello.pkg                            (macOS installer, signed + notarized)
        ├── Mello-0.2.0-osx-arm64-full.nupkg    (macOS full update package)
        ├── Mello-0.2.0-osx-arm64-delta.nupkg   (macOS delta update, if prev release exists)
        └── releases.osx-arm64.json              (macOS release index — Velopack reads this)
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
velopack = "0.0.914"       # Velopack Rust SDK
semver = "1"
tokio = { workspace = true }
```

The `self_update`, `sha2`, `hex`, `futures-util`, and `reqwest` (for update purposes) dependencies from the previous spec are **removed** — Velopack handles all of this internally.

### 4.2 Startup Hook

`VelopackApp::build().run()` **must** be the very first call in `main()`, before any other initialization. This is how Velopack handles install, uninstall, and update lifecycle hooks (e.g., creating/removing Start Menu shortcuts on Windows).

```rust
// client/src/main.rs

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ──── Velopack lifecycle hook — MUST be first ────
    velopack::VelopackApp::build().run();
    // ─────────────────────────────────────────────────

    env_logger::init();

    // ... rest of startup (Slint, updater, etc.)
}
```

If the process was spawned by Velopack for a lifecycle event (install, uninstall, update apply), `run()` performs the hook and exits. Otherwise it returns immediately and normal startup continues.

### 4.3 Core Types

```rust
// client/src/updater/mod.rs

/// Simplified update status for the UI layer.
/// Wraps Velopack's internal update info.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    /// Haven't checked yet
    Idle,

    /// Checking GitHub for updates
    Checking,

    /// No update available (already on latest)
    UpToDate,

    /// Update available, waiting for user action
    Available {
        version: String,
        /// Delta size if available, otherwise full size (bytes)
        download_size: u64,
    },

    /// Downloading update
    Downloading {
        progress: f32,      // 0.0 to 1.0
    },

    /// Downloaded, ready to install
    ReadyToInstall,

    /// Update failed
    Error(String),
}

/// Events emitted by updater for the UI
#[derive(Debug, Clone)]
pub enum UpdateEvent {
    CheckStarted,
    CheckComplete { update_available: bool },
    DownloadProgress { progress: f32 },
    DownloadComplete,
    InstallReady,
    Error(String),
}
```

### 4.4 Updater Implementation

```rust
// client/src/updater/updater.rs

use velopack::{UpdateManager, UpdateOptions, sources::GithubSource};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

const GITHUB_REPO_OWNER: &str = "mollohq";
const GITHUB_REPO_NAME: &str = "mello";

pub struct Updater {
    manager: UpdateManager<GithubSource>,
    status: Arc<RwLock<UpdateStatus>>,
    event_tx: mpsc::Sender<UpdateEvent>,
}

impl Updater {
    pub fn new(event_tx: mpsc::Sender<UpdateEvent>) -> Result<Self, Box<dyn std::error::Error>> {
        let source = GithubSource::new(
            &format!("https://github.com/{}/{}", GITHUB_REPO_OWNER, GITHUB_REPO_NAME),
            None,  // no pre-release filter
            false, // not pre-release channel
        );

        let manager = UpdateManager::new(
            source,
            None, // default options
            None, // default locator
        )?;

        Ok(Self {
            manager,
            status: Arc::new(RwLock::new(UpdateStatus::Idle)),
            event_tx,
        })
    }

    /// Get current status
    pub async fn status(&self) -> UpdateStatus {
        self.status.read().await.clone()
    }

    /// Check for updates (run in background)
    pub async fn check_for_updates(&self) -> Result<bool, Box<dyn std::error::Error>> {
        *self.status.write().await = UpdateStatus::Checking;
        self.event_tx.send(UpdateEvent::CheckStarted).await.ok();

        match self.manager.check_for_updates() {
            Ok(Some(update_info)) => {
                let version = update_info.target_full_release.version.to_string();
                let download_size = update_info.target_full_release.size as u64;

                *self.status.write().await = UpdateStatus::Available {
                    version: version.clone(),
                    download_size,
                };
                self.event_tx.send(UpdateEvent::CheckComplete { update_available: true }).await.ok();

                log::info!("Update available: v{}", version);
                Ok(true)
            }
            Ok(None) => {
                *self.status.write().await = UpdateStatus::UpToDate;
                self.event_tx.send(UpdateEvent::CheckComplete { update_available: false }).await.ok();
                log::info!("Already on latest version");
                Ok(false)
            }
            Err(e) => {
                let msg = format!("Update check failed: {}", e);
                *self.status.write().await = UpdateStatus::Error(msg.clone());
                self.event_tx.send(UpdateEvent::Error(msg)).await.ok();
                Err(e.into())
            }
        }
    }

    /// Download the update with progress reporting
    pub async fn download(&self) -> Result<(), Box<dyn std::error::Error>> {
        let update_info = self.manager.check_for_updates()?
            .ok_or("No update available")?;

        let status = self.status.clone();
        let tx = self.event_tx.clone();

        self.manager.download_updates(&update_info, Some(move |progress: i16| {
            let pct = progress as f32 / 100.0;
            // Fire-and-forget status update
            let _ = tx.try_send(UpdateEvent::DownloadProgress { progress: pct });
        }))?;

        *self.status.write().await = UpdateStatus::ReadyToInstall;
        self.event_tx.send(UpdateEvent::DownloadComplete).await.ok();

        Ok(())
    }

    /// Apply the downloaded update and restart the app.
    /// This function does not return on success — the process is replaced.
    pub fn apply_and_restart(&self) -> Result<(), Box<dyn std::error::Error>> {
        let update_info = self.manager.check_for_updates()?
            .ok_or("No update available")?;

        // This replaces the current process and restarts into the new version.
        self.manager.apply_updates_and_restart(&update_info, &[])?;

        // Should not reach here
        Ok(())
    }
}
```

### 4.5 Integration with App Startup

```rust
// client/src/main.rs

mod updater;

use updater::{Updater, UpdateEvent};
use tokio::sync::mpsc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ──── Velopack lifecycle hook — MUST be first ────
    velopack::VelopackApp::build().run();
    // ─────────────────────────────────────────────────

    env_logger::init();

    let rt = tokio::runtime::Runtime::new()?;

    let (event_tx, mut event_rx) = mpsc::channel::<UpdateEvent>(32);

    // Create updater (may fail if not installed via Velopack — dev mode)
    let updater = match Updater::new(event_tx) {
        Ok(u) => Some(u),
        Err(e) => {
            log::warn!("Updater init failed (dev mode?): {}", e);
            None
        }
    };

    // Check for updates in background
    if let Some(ref updater) = updater {
        let updater_ref = updater.clone();
        rt.spawn(async move {
            if let Err(e) = updater_ref.check_for_updates().await {
                log::warn!("Update check failed: {}", e);
            }
        });
    }

    // Continue with normal startup...
    let app = MainWindow::new()?;

    // Wire up update events to UI (poll event_rx in the Slint event loop)
    // ...

    app.run()?;

    Ok(())
}
```

---

## 5. UI Components

### 5.1 Update Banner

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

Two separate workflows — one for Windows, one for macOS. Each builds the client, packages with Velopack, signs, and uploads artifacts to a GitHub Release.

### 6.1 Windows Workflow

```yaml
# .github/workflows/release-windows.yml

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
            --packId Mello ^
            --packVersion %RELEASE_VERSION% ^
            --packDir dist ^
            --mainExe mello.exe ^
            --packTitle Mello ^
            --channel %RELEASE_CHANNEL% ^
            --outputDir vpk-out ^
            --icon assets/icons/app_icon.ico ^
            --azureTrustedSignFile windows/signing-metadata.json

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
# .github/workflows/release-macos.yml

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
            --packId Mello \
            --packVersion $RELEASE_VERSION \
            --packDir dist \
            --mainExe mello \
            --packTitle Mello \
            --channel $RELEASE_CHANNEL \
            --outputDir vpk-out \
            --icon assets/icons/app_icon.icns \
            --signAppIdentity "Developer ID Application: ${{ secrets.APPLE_TEAM_NAME }}" \
            --signInstallIdentity "Developer ID Installer: ${{ secrets.APPLE_TEAM_NAME }}" \
            --notaryProfile "AC_PASSWORD" \
            --signEntitlements macos/entitlements.plist

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
<!-- macos/entitlements.plist -->
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

### 8.1 Update Settings

```rust
// client/src/config.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    /// Check for updates on startup
    pub check_on_startup: bool,
    
    /// Automatically download updates (but don't install)
    pub auto_download: bool,
    
    /// Include pre-release versions
    pub include_prerelease: bool,
    
    /// Update channel
    pub channel: UpdateChannel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UpdateChannel {
    Stable,
    Beta,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_on_startup: true,
            auto_download: false,
            include_prerelease: false,
            channel: UpdateChannel::Stable,
        }
    }
}
```

Velopack channels map to the `--channel` flag used during `vpk pack`:

| UpdateChannel | Velopack channel (Windows) | Velopack channel (macOS) |
|---|---|---|
| Stable | `win-x64-stable` | `osx-arm64-stable` |
| Beta | `win-x64-beta` | `osx-arm64-beta` |

The channel determines which `releases.{channel}.json` the `UpdateManager` reads from GitHub Releases.

---

## 9. Testing Checklist

- [ ] `VelopackApp::build().run()` is the first call in `main()`
- [ ] Update check works on startup (background, non-blocking)
- [ ] UI shows update banner when update is available
- [ ] Download shows progress via Velopack callback
- [ ] "Update Now" triggers download, then shows "Restart"
- [ ] "Restart" calls `apply_updates_and_restart()` — app relaunches on new version
- [ ] Delta updates work when previous release exists
- [ ] Full update works for first install or when delta is unavailable
- [ ] "Later" dismisses banner until next startup
- [ ] Handles offline gracefully (update check fails silently)
- [ ] Updater init fails gracefully in dev mode (not installed via Velopack)
- [ ] **Windows:** Installer (`Mello-Setup.exe`) runs without SmartScreen warning
- [ ] **Windows:** Installed app has valid Authenticode signature (check with `signtool verify`)
- [ ] **macOS:** `.pkg` installer runs without Gatekeeper warning
- [ ] **macOS:** App binary passes `codesign --verify --deep --strict`
- [ ] **macOS:** App passes `spctl --assess --type install` (notarization check)
- [ ] **macOS:** `libonnxruntime.dylib` is correctly signed alongside the app

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

*This spec covers auto-updates, packaging, and code signing. For backend hosting, see [08-BACKEND-HOSTING.md](./08-BACKEND-HOSTING.md). For native platform integration, see [12-NATIVE-PLATFORM.md](./12-NATIVE-PLATFORM.md).*
