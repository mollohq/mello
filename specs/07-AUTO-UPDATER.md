# MELLO Auto-Updater Specification

> **Component:** Auto-Update System  
> **Version:** 0.2  
> **Status:** Planned  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

Mello includes a built-in auto-updater that checks for new versions on startup and allows users to update without leaving the app. Updates are distributed via GitHub Releases.

### Goals

- **Seamless:** Check on startup, non-blocking UI
- **User-controlled:** User decides when to install
- **Reliable:** Atomic updates with rollback capability
- **Transparent:** Show release notes and download progress

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
│   ┌─────────────────────┐                                               │
│   │ Check GitHub API    │◀──── GET /repos/mollohq/mello/releases/latest │
│   │ (background thread) │                                               │
│   └──────┬──────────────┘                                               │
│          │                                                              │
│          ▼                                                              │
│   ┌─────────────────────┐     ┌─────────────────────────────────────┐   │
│   │ Compare versions    │────▶│ v0.2.0 > v0.1.0 → Update available │   │
│   │ (semver)            │     │ v0.2.0 = v0.2.0 → Up to date       │   │
│   └──────┬──────────────┘     └─────────────────────────────────────┘   │
│          │                                                              │
│          ▼ (if update available)                                        │
│   ┌─────────────────────┐                                               │
│   │ Show banner in UI   │                                               │
│   │ "Update available"  │                                               │
│   └──────┬──────────────┘                                               │
│          │                                                              │
│          ▼ (user clicks "Update")                                       │
│   ┌─────────────────────┐                                               │
│   │ Download update     │──── Progress: 0% ─────▶ 100%                  │
│   │ (background)        │                                               │
│   └──────┬──────────────┘                                               │
│          │                                                              │
│          ▼                                                              │
│   ┌─────────────────────┐                                               │
│   │ Extract & verify    │                                               │
│   └──────┬──────────────┘                                               │
│          │                                                              │
│          ▼                                                              │
│   ┌─────────────────────┐                                               │
│   │ "Restart to update" │                                               │
│   └──────┬──────────────┘                                               │
│          │                                                              │
│          ▼ (user clicks "Restart")                                      │
│   ┌─────────────────────┐                                               │
│   │ Replace binary      │                                               │
│   │ Restart app         │                                               │
│   └─────────────────────┘                                               │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 3. GitHub Releases Structure

```
mollohq/mello
└── releases
    └── v0.2.0
        ├── mello-windows-x64.zip      (Windows build)
        ├── mello-macos-x64.zip        (macOS Intel)
        ├── mello-macos-arm64.zip      (macOS Apple Silicon)
        ├── mello-linux-x64.tar.gz     (Linux)
        ├── checksums.txt              (SHA256 for all files)
        └── RELEASE_NOTES.md           (Auto-generated or manual)
```

### Release Asset Naming Convention

```
mello-{os}-{arch}.{ext}

os:   windows | macos | linux
arch: x64 | arm64
ext:  zip (Windows/macOS) | tar.gz (Linux)
```

---

## 4. Implementation

### 4.1 Dependencies

```toml
# client/Cargo.toml

[dependencies]
self_update = { version = "0.39", features = ["archive-zip", "archive-tar"] }
semver = "1"
reqwest = { version = "0.11", features = ["json", "stream"] }
tokio = { workspace = true }
futures-util = "0.3"
sha2 = "0.10"           # Checksum verification
hex = "0.4"
```

### 4.2 Core Types

```rust
// client/src/updater/mod.rs

use semver::Version;
use std::path::PathBuf;

/// Update information from GitHub
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current_version: Version,
    pub latest_version: Version,
    pub download_url: String,
    pub download_size: u64,
    pub checksum: Option<String>,
    pub release_notes: String,
    pub published_at: String,
}

/// Current state of the updater
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    /// Haven't checked yet
    Idle,
    
    /// Checking GitHub for updates
    Checking,
    
    /// No update available (already on latest)
    UpToDate,
    
    /// Update available, waiting for user action
    Available(UpdateInfo),
    
    /// Downloading update
    Downloading { 
        progress: f32,      // 0.0 to 1.0
        bytes_downloaded: u64,
        total_bytes: u64,
    },
    
    /// Downloaded, ready to install
    ReadyToInstall(PathBuf),
    
    /// Installing (replacing binary)
    Installing,
    
    /// Update failed
    Error(String),
}

/// Events emitted by updater
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

### 4.3 Updater Implementation

```rust
// client/src/updater/updater.rs

use super::*;
use reqwest::Client;
use semver::Version;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use futures_util::StreamExt;

const GITHUB_REPO_OWNER: &str = "mollohq";
const GITHUB_REPO_NAME: &str = "mello";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Updater {
    status: Arc<RwLock<UpdateStatus>>,
    event_tx: mpsc::Sender<UpdateEvent>,
    event_rx: mpsc::Receiver<UpdateEvent>,
    http_client: Client,
    download_dir: PathBuf,
}

impl Updater {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(32);
        
        // Download to temp directory
        let download_dir = std::env::temp_dir().join("mello_update");
        std::fs::create_dir_all(&download_dir).ok();
        
        Self {
            status: Arc::new(RwLock::new(UpdateStatus::Idle)),
            event_tx,
            event_rx,
            http_client: Client::new(),
            download_dir,
        }
    }
    
    /// Get current status
    pub async fn status(&self) -> UpdateStatus {
        self.status.read().await.clone()
    }
    
    /// Poll for events (non-blocking)
    pub fn poll_event(&mut self) -> Option<UpdateEvent> {
        self.event_rx.try_recv().ok()
    }
    
    /// Check for updates (run in background)
    pub async fn check_for_updates(&self) -> Result<Option<UpdateInfo>, UpdateError> {
        *self.status.write().await = UpdateStatus::Checking;
        self.event_tx.send(UpdateEvent::CheckStarted).await.ok();
        
        let current = Version::parse(CURRENT_VERSION)?;
        
        // Fetch latest release from GitHub
        let url = format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            GITHUB_REPO_OWNER,
            GITHUB_REPO_NAME
        );
        
        let response: GitHubRelease = self.http_client
            .get(&url)
            .header("User-Agent", format!("mello/{}", CURRENT_VERSION))
            .send()
            .await?
            .json()
            .await?;
        
        // Parse version (strip 'v' prefix if present)
        let version_str = response.tag_name.strip_prefix('v')
            .unwrap_or(&response.tag_name);
        let latest = Version::parse(version_str)?;
        
        if latest <= current {
            *self.status.write().await = UpdateStatus::UpToDate;
            self.event_tx.send(UpdateEvent::CheckComplete { update_available: false }).await.ok();
            log::info!("Already on latest version: {}", current);
            return Ok(None);
        }
        
        // Find the right asset for this platform
        let asset = self.find_platform_asset(&response.assets)?;
        
        // Try to get checksum
        let checksum = self.fetch_checksum(&response.assets, &asset.name).await.ok();
        
        let info = UpdateInfo {
            current_version: current,
            latest_version: latest.clone(),
            download_url: asset.browser_download_url.clone(),
            download_size: asset.size,
            checksum,
            release_notes: response.body.unwrap_or_default(),
            published_at: response.published_at,
        };
        
        *self.status.write().await = UpdateStatus::Available(info.clone());
        self.event_tx.send(UpdateEvent::CheckComplete { update_available: true }).await.ok();
        
        log::info!("Update available: {} -> {}", current, latest);
        Ok(Some(info))
    }
    
    /// Download the update
    pub async fn download(&self) -> Result<PathBuf, UpdateError> {
        let info = match &*self.status.read().await {
            UpdateStatus::Available(info) => info.clone(),
            _ => return Err(UpdateError::NoUpdateAvailable),
        };
        
        let file_name = info.download_url.split('/').last()
            .ok_or_else(|| UpdateError::InvalidUrl)?;
        let file_path = self.download_dir.join(file_name);
        
        log::info!("Downloading update from: {}", info.download_url);
        
        let response = self.http_client
            .get(&info.download_url)
            .header("User-Agent", format!("mello/{}", CURRENT_VERSION))
            .send()
            .await?;
        
        let total_size = response.content_length().unwrap_or(info.download_size);
        let mut downloaded: u64 = 0;
        
        let mut file = tokio::fs::File::create(&file_path).await?;
        let mut stream = response.bytes_stream();
        
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            downloaded += chunk.len() as u64;
            
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            
            let progress = downloaded as f32 / total_size as f32;
            
            *self.status.write().await = UpdateStatus::Downloading {
                progress,
                bytes_downloaded: downloaded,
                total_bytes: total_size,
            };
            
            self.event_tx.send(UpdateEvent::DownloadProgress { progress }).await.ok();
        }
        
        // Verify checksum if available
        if let Some(expected) = &info.checksum {
            log::info!("Verifying checksum...");
            let actual = self.compute_checksum(&file_path).await?;
            if &actual != expected {
                return Err(UpdateError::ChecksumMismatch {
                    expected: expected.clone(),
                    actual,
                });
            }
            log::info!("Checksum verified!");
        }
        
        *self.status.write().await = UpdateStatus::ReadyToInstall(file_path.clone());
        self.event_tx.send(UpdateEvent::DownloadComplete).await.ok();
        
        Ok(file_path)
    }
    
    /// Install the update (replaces current binary, then restarts)
    pub async fn install(&self) -> Result<(), UpdateError> {
        let archive_path = match &*self.status.read().await {
            UpdateStatus::ReadyToInstall(path) => path.clone(),
            _ => return Err(UpdateError::NotReadyToInstall),
        };
        
        *self.status.write().await = UpdateStatus::Installing;
        
        log::info!("Installing update from: {:?}", archive_path);
        
        // Use self_update to handle the atomic replacement
        self_update::self_replace::self_replace(&archive_path)?;
        
        // Restart the application
        self.restart()?;
        
        Ok(())
    }
    
    /// Find the appropriate asset for this platform
    fn find_platform_asset(&self, assets: &[GitHubAsset]) -> Result<&GitHubAsset, UpdateError> {
        let target = if cfg!(target_os = "windows") {
            if cfg!(target_arch = "x86_64") {
                "mello-windows-x64.zip"
            } else {
                "mello-windows-arm64.zip"
            }
        } else if cfg!(target_os = "macos") {
            if cfg!(target_arch = "x86_64") {
                "mello-macos-x64.zip"
            } else {
                "mello-macos-arm64.zip"
            }
        } else if cfg!(target_os = "linux") {
            "mello-linux-x64.tar.gz"
        } else {
            return Err(UpdateError::UnsupportedPlatform);
        };
        
        assets.iter()
            .find(|a| a.name == target)
            .ok_or(UpdateError::NoAssetForPlatform)
    }
    
    /// Fetch checksum from checksums.txt if available
    async fn fetch_checksum(&self, assets: &[GitHubAsset], target_name: &str) -> Result<String, UpdateError> {
        let checksums_asset = assets.iter()
            .find(|a| a.name == "checksums.txt")
            .ok_or(UpdateError::NoChecksumFile)?;
        
        let content = self.http_client
            .get(&checksums_asset.browser_download_url)
            .header("User-Agent", format!("mello/{}", CURRENT_VERSION))
            .send()
            .await?
            .text()
            .await?;
        
        // Format: "sha256hash  filename"
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 2 && parts[1] == target_name {
                return Ok(parts[0].to_string());
            }
        }
        
        Err(UpdateError::NoChecksumForFile)
    }
    
    /// Compute SHA256 checksum of a file
    async fn compute_checksum(&self, path: &PathBuf) -> Result<String, UpdateError> {
        use sha2::{Sha256, Digest};
        
        let data = tokio::fs::read(path).await?;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let result = hasher.finalize();
        
        Ok(hex::encode(result))
    }
    
    /// Restart the application
    fn restart(&self) -> Result<(), UpdateError> {
        let exe = std::env::current_exe()?;
        let args: Vec<String> = std::env::args().skip(1).collect();
        
        std::process::Command::new(&exe)
            .args(&args)
            .spawn()?;
        
        std::process::exit(0);
    }
}

// GitHub API types
#[derive(Debug, serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
    body: Option<String>,
    published_at: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, serde::Deserialize)]
struct GitHubAsset {
    name: String,
    size: u64,
    browser_download_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Version parse error: {0}")]
    Version(#[from] semver::Error),
    
    #[error("No update available")]
    NoUpdateAvailable,
    
    #[error("Not ready to install")]
    NotReadyToInstall,
    
    #[error("Invalid download URL")]
    InvalidUrl,
    
    #[error("Unsupported platform")]
    UnsupportedPlatform,
    
    #[error("No asset available for this platform")]
    NoAssetForPlatform,
    
    #[error("No checksum file found")]
    NoChecksumFile,
    
    #[error("No checksum for this file")]
    NoChecksumForFile,
    
    #[error("Checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    
    #[error("Self-update error: {0}")]
    SelfUpdate(#[from] self_update::errors::Error),
}
```

### 4.4 Integration with App Startup

```rust
// client/src/main.rs

mod updater;

use updater::Updater;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    
    // Create updater
    let updater = Arc::new(RwLock::new(Updater::new()));
    
    // Check for updates in background
    let updater_clone = updater.clone();
    tokio::spawn(async move {
        let updater = updater_clone.read().await;
        match updater.check_for_updates().await {
            Ok(Some(info)) => {
                log::info!("Update available: v{}", info.latest_version);
            }
            Ok(None) => {
                log::info!("Already on latest version");
            }
            Err(e) => {
                log::warn!("Update check failed: {}", e);
            }
        }
    });
    
    // Continue with normal startup...
    let app = MainWindow::new()?;
    
    // Wire up update events to UI
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

---

## 6. GitHub Actions: Build & Release

```yaml
# .github/workflows/release.yml

name: Release

on:
  push:
    tags:
      - 'v*'

permissions:
  contents: write

jobs:
  build:
    strategy:
      matrix:
        include:
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            artifact: mello-windows-x64.zip
          # - os: macos-latest
          #   target: x86_64-apple-darwin
          #   artifact: mello-macos-x64.zip
          # - os: macos-latest
          #   target: aarch64-apple-darwin
          #   artifact: mello-macos-arm64.zip
    
    runs-on: ${{ matrix.os }}
    
    steps:
      - uses: actions/checkout@v4
        with:
          submodules: recursive
      
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
      
      - uses: Swatinem/rust-cache@v2
      
      # Build libmello
      - name: Build libmello
        shell: bash
        run: |
          cd libmello
          cmake -B build -DCMAKE_BUILD_TYPE=Release
          cmake --build build --config Release
      
      # Build client
      - name: Build client
        run: cargo build --release --target ${{ matrix.target }}
      
      # Package
      - name: Package (Windows)
        if: matrix.os == 'windows-latest'
        shell: pwsh
        run: |
          mkdir dist
          cp target/${{ matrix.target }}/release/mello.exe dist/
          cp libmello/build/Release/*.dll dist/
          Compress-Archive -Path dist/* -DestinationPath ${{ matrix.artifact }}
      
      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: ${{ matrix.artifact }}
          path: ${{ matrix.artifact }}

  release:
    needs: build
    runs-on: ubuntu-latest
    
    steps:
      - uses: actions/checkout@v4
      
      - uses: actions/download-artifact@v4
        with:
          path: artifacts
      
      # Generate checksums
      - name: Generate checksums
        run: |
          cd artifacts
          find . -type f \( -name "*.zip" -o -name "*.tar.gz" \) -exec sha256sum {} \; > checksums.txt
          cat checksums.txt
      
      # Create release
      - name: Create Release
        uses: softprops/action-gh-release@v2
        with:
          draft: false
          generate_release_notes: true
          files: |
            artifacts/**/*.zip
            artifacts/**/*.tar.gz
            artifacts/checksums.txt
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

---

## 7. Configuration

### 7.1 Update Settings

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
    Nightly,
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

---

## 8. Testing Checklist

- [ ] Update check works on startup (background)
- [ ] UI shows update banner when available
- [ ] Download shows progress accurately
- [ ] Download can be cancelled
- [ ] Checksum verification works
- [ ] Install replaces binary correctly
- [ ] Restart launches new version
- [ ] Rollback works if install fails
- [ ] "Later" dismisses banner until next startup
- [ ] Works behind proxies
- [ ] Handles offline gracefully

---

## 9. Security Considerations

| Concern | Mitigation |
|---------|------------|
| MITM attacks | HTTPS only, checksum verification |
| Binary tampering | SHA256 checksums in release |
| Privilege escalation | No admin required on Windows |
| Rollback attacks | Only allow updating to newer versions |

---

## 10. Rollback Strategy

If an update causes issues:

1. **Manual rollback:** Download previous version from GitHub releases
2. **Future:** Keep previous version and add "Rollback" button in settings

---

*This spec covers auto-updates. For backend hosting, see [08-BACKEND-HOSTING.md](./08-BACKEND-HOSTING.md).*
