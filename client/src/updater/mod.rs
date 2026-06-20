pub(crate) mod startup_update;
#[allow(clippy::module_inception)]
mod updater;

pub use updater::Updater;

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
