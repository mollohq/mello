//! Minidump on a hung native teardown.
//!
//! When a native stop passes its deadline (`mello_core::stream::teardown`),
//! the log names the step, but not the call inside libmello or the driver that
//! blocks. A minidump of the thread stacks does. One dump per process run, to
//! the logs folder, so a user can attach it with the logs.

use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
static WRITTEN: AtomicBool = AtomicBool::new(false);

/// `MiniDumpWithThreadInfo`. With `MiniDumpNormal` (value 0) implied, this is
/// thread stacks and thread state, no heap. Small enough to attach to a log.
const DUMP_TYPE: u32 = 0x0000_1000;

/// Install the teardown hang hook. Call once at startup, after logging.
pub fn install(log_dir: Option<PathBuf>) {
    if let Some(dir) = log_dir {
        let _ = LOG_DIR.set(dir);
    }
    mello_core::stream::teardown::set_hang_hook(|hang| {
        write_dump(hang.label, hang.step);
    });
}

fn write_dump(label: &str, step: &str) {
    if WRITTEN.swap(true, Ordering::AcqRel) {
        log::info!("hang dump: already written this session, skipping ({label}/{step})");
        return;
    }
    let Some(dir) = LOG_DIR.get() else {
        log::warn!("hang dump: no log folder, skipping");
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("hang-{label}-{stamp}.dmp"));
    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("hang dump: cannot create {}: {}", path.display(), e);
            return;
        }
    };
    let ok = unsafe {
        MiniDumpWriteDump(
            GetCurrentProcess(),
            GetCurrentProcessId(),
            file.as_raw_handle(),
            DUMP_TYPE,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok != 0 {
        log::error!(
            "hang dump: wrote {} for teardown '{}' stuck in '{}'",
            path.display(),
            label,
            step
        );
    } else {
        log::warn!(
            "hang dump: MiniDumpWriteDump failed: {}",
            std::io::Error::last_os_error()
        );
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}

#[link(name = "dbghelp")]
extern "system" {
    fn MiniDumpWriteDump(
        process: *mut std::ffi::c_void,
        process_id: u32,
        file: *mut std::ffi::c_void,
        dump_type: u32,
        exception_param: *const std::ffi::c_void,
        user_stream_param: *const std::ffi::c_void,
        callback_param: *const std::ffi::c_void,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn GetCurrentProcessId() -> u32;
}
