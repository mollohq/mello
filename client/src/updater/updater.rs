use velopack::sources::{FileSource, HttpSource};
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackApp};
use std::sync::mpsc;

use super::{UpdateEvent, UpdateStatus};

const GITHUB_RELEASES_URL: &str =
    "https://github.com/mollohq/mello/releases/latest/download/";

pub struct Updater {
    manager: UpdateManager,
    status: UpdateStatus,
    event_tx: mpsc::Sender<UpdateEvent>,
    cached_update: Option<UpdateInfo>,
}

impl Updater {
    /// Must be called as the very first thing in main(), before any other init.
    /// Handles Velopack install/uninstall/update lifecycle hooks.
    pub fn run_lifecycle_hooks() {
        VelopackApp::build().run();
    }

    pub fn new(event_tx: mpsc::Sender<UpdateEvent>) -> Result<Self, Box<dyn std::error::Error>> {
        let update_url = std::env::var("MELLO_UPDATE_URL").ok();
        let manager = match update_url.as_deref() {
            Some(url) if url.starts_with("http") => {
                log::info!("Update source override (HTTP): {}", url);
                UpdateManager::new(HttpSource::new(url), None, None)?
            }
            Some(path) => {
                log::info!("Update source override (local): {}", path);
                UpdateManager::new(FileSource::new(path), None, None)?
            }
            None => {
                UpdateManager::new(HttpSource::new(GITHUB_RELEASES_URL), None, None)?
            }
        };

        log::info!(
            "Updater initialized — current version: {}",
            manager.get_current_version_as_string()
        );

        Ok(Self {
            manager,
            status: UpdateStatus::Idle,
            event_tx,
            cached_update: None,
        })
    }

    #[allow(dead_code)]
    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    pub fn current_version(&self) -> String {
        self.manager.get_current_version_as_string()
    }

    /// Check for updates. Returns true if an update is available.
    pub fn check_for_updates(&mut self) -> bool {
        self.status = UpdateStatus::Checking;
        self.event_tx.send(UpdateEvent::CheckStarted).ok();

        match self.manager.check_for_updates() {
            Ok(UpdateCheck::UpdateAvailable(info)) => {
                let version = info.TargetFullRelease.Version.clone();
                let size = info.TargetFullRelease.Size;

                log::info!("Update available: v{} ({} bytes)", version, size);

                self.status = UpdateStatus::Available {
                    version: version.clone(),
                    download_size: size,
                };
                self.event_tx
                    .send(UpdateEvent::CheckComplete {
                        update_available: true,
                        version: Some(version),
                        download_size: Some(size),
                    })
                    .ok();
                self.cached_update = Some(info);
                true
            }
            Ok(UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty) => {
                log::info!("Already on latest version");
                self.status = UpdateStatus::UpToDate;
                self.event_tx
                    .send(UpdateEvent::CheckComplete {
                        update_available: false,
                        version: None,
                        download_size: None,
                    })
                    .ok();
                self.cached_update = None;
                false
            }
            Err(e) => {
                let msg = format!("Update check failed: {}", e);
                log::warn!("{}", msg);
                self.status = UpdateStatus::Error(msg.clone());
                self.event_tx.send(UpdateEvent::Error(msg)).ok();
                self.cached_update = None;
                false
            }
        }
    }

    /// Download the pending update. Must call check_for_updates first.
    pub fn download(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let info = self
            .cached_update
            .as_ref()
            .ok_or("No update available — call check_for_updates first")?;

        let (progress_tx, progress_rx) = std::sync::mpsc::channel::<i16>();

        // Forward progress updates to our event channel
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            while let Ok(pct) = progress_rx.recv() {
                let progress = pct as f32 / 100.0;
                event_tx
                    .send(UpdateEvent::DownloadProgress { progress })
                    .ok();
            }
        });

        self.status = UpdateStatus::Downloading { progress: 0.0 };

        self.manager.download_updates(info, Some(progress_tx))?;

        self.status = UpdateStatus::ReadyToInstall;
        self.event_tx.send(UpdateEvent::DownloadComplete).ok();
        Ok(())
    }

    /// Apply the downloaded update and restart the app.
    /// This replaces the current process on success — does not return.
    pub fn apply_and_restart(&self) -> Result<(), Box<dyn std::error::Error>> {
        let info = self
            .cached_update
            .as_ref()
            .ok_or("No update available to apply")?;

        log::info!("Applying update and restarting...");
        self.manager.apply_updates_and_restart(info)?;

        Ok(())
    }
}
