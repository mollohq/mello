mod updater;

pub use updater::Updater;

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    UpToDate,
    Available {
        version: String,
        download_size: u64,
    },
    Downloading {
        progress: f32,
    },
    ReadyToInstall,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum UpdateEvent {
    CheckStarted,
    CheckComplete { update_available: bool, version: Option<String>, download_size: Option<u64> },
    DownloadProgress { progress: f32 },
    DownloadComplete,
    Error(String),
}
