//! Client-side diagnostic capture: drives the runtime log-verbosity bump and
//! slices the captured window out of the rolling log file for upload.
//!
//! The actual subscriber `reload::Handle` is owned by `init_logging`; it hands
//! us a toggle closure via [`install_log_control`]. Capture state (which file
//! we're tailing and from what offset) lives in a UI-thread-local since the
//! whole capture flow runs on the Slint event loop.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Verbosity directive applied while a capture is active. Targeted so the
/// captured window stays focused on the audio/voice/reconnect paths rather than
/// every dependency at debug.
pub const CAPTURE_DIRECTIVE: &str = "info,audio_stats=debug,libmello=debug,mello_core=debug";

/// Flips the subscriber's `EnvFilter` between the base directive and
/// [`CAPTURE_DIRECTIVE`]. `true` = verbose capture, `false` = restore base.
pub type FilterToggle = Box<dyn Fn(bool) + Send + Sync>;

static LOG_VERBOSITY: OnceLock<FilterToggle> = OnceLock::new();
static LOG_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

struct Active {
    capture_id: String,
    file: PathBuf,
    offset: u64,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

/// Called once from `init_logging` to hand over the verbosity toggle + log dir.
pub fn install_log_control(toggle: FilterToggle, log_dir: Option<PathBuf>) {
    let _ = LOG_VERBOSITY.set(toggle);
    let _ = LOG_DIR.set(log_dir);
}

fn set_verbose(verbose: bool) {
    if let Some(toggle) = LOG_VERBOSITY.get() {
        toggle(verbose);
    }
}

/// Begin a capture: raise verbosity and remember the current log file + its
/// length so we can slice exactly the capture window later. Returns the new
/// capture id, or `None` if no log file is available to tail (upload would have
/// nothing to send).
pub fn begin() -> Option<String> {
    let capture_id = format!(
        "diag_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    let file = current_log_file()?;
    let offset = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);

    set_verbose(true);

    ACTIVE.with(|a| {
        *a.borrow_mut() = Some(Active {
            capture_id: capture_id.clone(),
            file,
            offset,
        });
    });
    Some(capture_id)
}

/// Whether a capture is currently active.
pub fn is_active() -> bool {
    ACTIVE.with(|a| a.borrow().is_some())
}

/// End a capture: restore verbosity, slice `[offset..end]` of the tailed log
/// into a temp file, and return `(slice_path, capture_id)` for upload. Returns
/// `None` if there was no active capture or the slice could not be written.
pub fn finish() -> Option<(PathBuf, String)> {
    set_verbose(false);

    let active = ACTIVE.with(|a| a.borrow_mut().take())?;

    // Prefer the originally-tailed file; if a daily rollover happened mid-capture
    // (rare; captures are short), fall back to the newest file and take it whole.
    let (source, start) = if active.file.exists() {
        (active.file.clone(), active.offset)
    } else {
        (current_log_file()?, 0)
    };

    let slice = read_slice(&source, start)?;

    let out = std::env::temp_dir().join(format!("{}.log", active.capture_id));
    if let Err(e) = std::fs::write(&out, slice) {
        log::warn!(
            "diag capture: failed to write slice {}: {}",
            out.display(),
            e
        );
        return None;
    }
    Some((out, active.capture_id))
}

/// Read a file from byte `start` to end. Caps the slice so an unexpectedly huge
/// log can't produce an unbounded upload.
fn read_slice(path: &Path, start: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    const MAX_SLICE_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB safety cap

    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("diag capture: cannot open {}: {}", path.display(), e);
            return None;
        }
    };
    let len = f.metadata().ok()?.len();
    let start = start.min(len);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return None;
    }
    let to_read = (len - start).min(MAX_SLICE_BYTES);
    let mut buf = Vec::with_capacity(to_read as usize);
    if f.take(to_read).read_to_end(&mut buf).is_err() {
        return None;
    }
    Some(buf)
}

/// The active rolling log file: newest file in the log dir whose name starts
/// with the `mello.log` prefix used by the daily appender.
fn current_log_file() -> Option<PathBuf> {
    let dir = LOG_DIR.get().and_then(|d| d.clone())?;
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        let is_log = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("mello.log"))
            .unwrap_or(false);
        if !is_log {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
        if newest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, p)| p)
}
