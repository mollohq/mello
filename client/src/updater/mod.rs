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
    DownloadProgress {
        progress: f32,
    },
    Error(String),
}
