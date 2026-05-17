use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use velopack::sources::{FileSource, HttpSource};
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackApp};

use super::UpdateEvent;

const GITHUB_RELEASES_URL: &str = "https://github.com/mollohq/mello/releases/latest/download/";

pub struct Updater {
    manager: UpdateManager,
    event_tx: mpsc::Sender<UpdateEvent>,
    cached_update: Option<UpdateInfo>,
    /// Prevents overlapping Velopack download/apply (exclusive lock); cleared on error.
    update_job_active: Arc<AtomicBool>,
}

impl Updater {
    /// Must be called as the very first thing in main(), before any other init.
    pub fn run_lifecycle_hooks() {
        let mut app = VelopackApp::build();

        #[cfg(target_os = "windows")]
        {
            app = app
                .on_after_install_fast_callback(|_version| {
                    Self::register_url_protocol();
                })
                .on_after_update_fast_callback(|_version| {
                    Self::register_url_protocol();
                })
                .on_before_uninstall_fast_callback(|_version| {
                    Self::unregister_url_protocol();
                });
        }

        app.run();
    }

    #[cfg(target_os = "windows")]
    fn register_url_protocol() {
        use winreg::enums::*;
        use winreg::RegKey;

        let exe = std::env::current_exe().unwrap_or_default();
        let exe_str = exe.to_string_lossy();

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok((key, _)) = hkcu.create_subkey("Software\\Classes\\mello") {
            let _ = key.set_value("", &"URL:mello Protocol");
            let _ = key.set_value("URL Protocol", &"");
            if let Ok((cmd_key, _)) = key.create_subkey("shell\\open\\command") {
                let _ = cmd_key.set_value("", &format!("\"{}\" \"%1\"", exe_str));
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn unregister_url_protocol() {
        use winreg::enums::*;
        use winreg::RegKey;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let _ = hkcu.delete_subkey_all("Software\\Classes\\mello");
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
            None => UpdateManager::new(HttpSource::new(GITHUB_RELEASES_URL), None, None)?,
        };

        log::info!(
            "Updater initialized — current version: {}",
            manager.get_current_version_as_string()
        );

        Ok(Self {
            manager,
            event_tx,
            cached_update: None,
            update_job_active: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn current_version(&self) -> String {
        self.manager.get_current_version_as_string()
    }

    /// Check for updates. Returns true if an update is available.
    pub fn check_for_updates(&mut self) -> bool {
        self.event_tx.send(UpdateEvent::CheckStarted).ok();

        match self.manager.check_for_updates() {
            Ok(UpdateCheck::UpdateAvailable(info)) => {
                let version = info.TargetFullRelease.Version.clone();
                let size = info.TargetFullRelease.Size;

                log::info!("Update available: v{} ({} bytes)", version, size);

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
                self.event_tx.send(UpdateEvent::Error(msg)).ok();
                self.cached_update = None;
                false
            }
        }
    }

    /// Starts download and apply/restart on a background thread so the UI thread stays responsive.
    /// On success the process is replaced and this never completes on that path.
    pub fn update_and_restart(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let info = self
            .cached_update
            .as_ref()
            .ok_or("No update available — call check_for_updates first")?;

        if self
            .update_job_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            log::warn!("Update already in progress");
            return Err("Update already in progress".into());
        }

        let manager = self.manager.clone();
        let info = info.clone();
        let event_tx = self.event_tx.clone();
        let guard = Arc::clone(&self.update_job_active);

        std::thread::spawn(move || {
            let (progress_tx, progress_rx) = mpsc::channel::<i16>();
            let event_tx_progress = event_tx.clone();
            std::thread::spawn(move || {
                while let Ok(pct) = progress_rx.recv() {
                    let progress = pct as f32 / 100.0;
                    event_tx_progress
                        .send(UpdateEvent::DownloadProgress { progress })
                        .ok();
                }
            });

            if let Err(e) = manager.download_updates(&info, Some(progress_tx)) {
                log::warn!("Update download failed: {}", e);
                let _ = event_tx.send(UpdateEvent::Error(format!("Update download failed: {}", e)));
                guard.store(false, Ordering::SeqCst);
                return;
            }

            log::info!("Download complete, applying update and restarting...");
            if let Err(e) = manager.apply_updates_and_restart(&info) {
                log::warn!("Apply update / restart failed: {}", e);
                let _ = event_tx.send(UpdateEvent::Error(format!("Apply update failed: {}", e)));
                guard.store(false, Ordering::SeqCst);
            }
        });

        Ok(())
    }
}
