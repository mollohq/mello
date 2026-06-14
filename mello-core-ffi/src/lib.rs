//! C-ABI for `mello-core`.
//!
//! The boundary is intentionally narrow: a single opaque handle wraps a running
//! `mello_core::Client` plus its tokio runtime. The UI drives it by sending
//! serialized `Command` JSON in (`mello_core_send_command`) and receiving
//! serialized `Event` JSON out via a registered callback. Both enums are
//! adjacently-tagged serde types (`{ "type": ..., "data": ... }`), so adding a
//! variant never changes this ABI. See `mello-ios/specs/IOS-02-CORE-FFI.md`.
//!
//! Media (voice/video packets) never crosses this boundary; it stays inside
//! libmello and the SFU connection.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use mello_core::{
    Client, Command, Event, FrameLifecycleSlot, FrameSlot, NativeFrameSlot, FRAME_STATE_PRESENTED,
};

/// Opaque handle: owns the core `Client`, its tokio runtime, and the event pump.
pub struct MelloCoreHandle {
    // Held to keep the runtime (and the spawned `Client::run` task) alive for the
    // handle's lifetime; only ever touched via `Drop` in `mello_core_destroy`.
    #[allow(dead_code)]
    rt: tokio::runtime::Runtime,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Command>,
    pump: Option<JoinHandle<()>>,
}

/// Event delivery callback. Invoked on the dedicated pump thread (never main).
/// `event_json` is owned by Rust and valid only for the duration of the call;
/// the callee must copy it before returning.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MelloEventCallback {
    pub callback: extern "C" fn(event_json: *const c_char, user_data: *mut c_void),
    pub user_data: *mut c_void,
}

/// Install a process-wide logger once. iOS apps have no stdout/stderr console, so
/// route the `log` crate (mello-core records + libmello logs bridged via
/// `mello_set_log_callback`) to the unified logging system: visible in Xcode's
/// console, Console.app, and `log stream`. No-op on non-iOS hosts (the
/// examples/tests install their own `env_logger`).
fn init_logging() {
    use std::sync::Once;
    static LOG_INIT: Once = Once::new();
    LOG_INIT.call_once(|| {
        #[cfg(target_os = "ios")]
        {
            let _ = oslog::OsLogger::new("app.m3llo.client-ios")
                .level_filter(log::LevelFilter::Info)
                .init();
        }
    });
}

/// Send wrapper so the callback + user_data can move into the pump thread.
/// Safety: the Swift side keeps `user_data` (an `Unmanaged<MelloCore>`) alive for
/// the lifetime of the handle and the callback is thread-safe on its side.
struct CallbackCtx {
    callback: extern "C" fn(*const c_char, *mut c_void),
    user_data: *mut c_void,
}
unsafe impl Send for CallbackCtx {}

impl CallbackCtx {
    /// Serialize-and-deliver one event. Called only on the pump thread.
    /// Taking `&self` forces the pump closure to capture the whole (Send)
    /// `CallbackCtx` rather than its individual fields (Rust 2021 disjoint
    /// capture would otherwise grab the `!Send` `user_data` directly).
    fn emit(&self, json: &str) {
        match CString::new(json) {
            Ok(cstr) => (self.callback)(cstr.as_ptr(), self.user_data),
            Err(e) => log::warn!("event pump: nul byte in json: {e}"),
        }
    }
}

/// Create and start the core. `config_json` is a serialized `mello_core::Config`.
/// Returns null on invalid config or runtime-creation failure.
///
/// # Safety
/// `config_json` must be a valid, NUL-terminated C string (or null). `cb.callback`
/// must be a valid function pointer and `cb.user_data` must remain valid until
/// `mello_core_destroy` is called on the returned handle.
#[no_mangle]
pub unsafe extern "C" fn mello_core_create(
    config_json: *const c_char,
    cb: MelloEventCallback,
) -> *mut MelloCoreHandle {
    init_logging();
    if config_json.is_null() {
        log::error!("mello_core_create: null config_json");
        return std::ptr::null_mut();
    }
    let cfg_str = CStr::from_ptr(config_json).to_string_lossy().into_owned();
    let config: mello_core::Config = match serde_json::from_str(&cfg_str) {
        Ok(c) => c,
        Err(e) => {
            log::error!("mello_core_create: invalid config json: {e}");
            return std::ptr::null_mut();
        }
    };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("mello_core_create: tokio runtime failed: {e}");
            return std::ptr::null_mut();
        }
    };

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();

    // Frame slots are required by `Client::new` for the desktop stream-viewer
    // GPU handoff. The iOS viewer path is wired separately (IOS-06); here they
    // are inert placeholders.
    let frame_slot: FrameSlot = Arc::new(Mutex::new(None));
    let native_frame_slot: NativeFrameSlot = Arc::new(Mutex::new(None));
    let frame_consumed = Arc::new(AtomicBool::new(true));
    let frame_lifecycle: FrameLifecycleSlot = Arc::new(AtomicU8::new(FRAME_STATE_PRESENTED));

    rt.spawn(async move {
        // Game sensing is desktop process-scanning; disabled on the FFI path.
        let mut client = Client::new_with_game_sensor(
            config,
            event_tx,
            false, // loopback
            frame_slot,
            native_frame_slot,
            frame_consumed,
            frame_lifecycle,
            false, // enable_game_sensor
        );
        client.run(cmd_rx).await;
    });

    let ctx = CallbackCtx {
        callback: cb.callback,
        user_data: cb.user_data,
    };
    let pump = std::thread::Builder::new()
        .name("mello-event-pump".into())
        .spawn(move || {
            // Exits when the core drops `event_tx` (on shutdown).
            while let Ok(event) = event_rx.recv() {
                match serde_json::to_string(&event) {
                    Ok(json) => ctx.emit(&json),
                    Err(e) => log::warn!("event pump: serialize failed: {e}"),
                }
            }
            log::info!("event pump stopped");
        })
        .expect("spawn event pump thread");

    Box::into_raw(Box::new(MelloCoreHandle {
        rt,
        cmd_tx,
        pump: Some(pump),
    }))
}

/// Enqueue a serialized `Command`. Returns 0 on success, negative on error:
/// -1 null arg, -2 bad JSON, -3 channel full/closed.
///
/// # Safety
/// `handle` must be a live pointer from `mello_core_create` and `command_json` a
/// valid NUL-terminated C string (or null).
#[no_mangle]
pub unsafe extern "C" fn mello_core_send_command(
    handle: *mut MelloCoreHandle,
    command_json: *const c_char,
) -> i32 {
    if handle.is_null() || command_json.is_null() {
        return -1;
    }
    let handle = &*handle;
    let cmd_str = CStr::from_ptr(command_json).to_string_lossy().into_owned();
    let cmd: Command = match serde_json::from_str(&cmd_str) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("mello_core_send_command: bad json: {e}");
            return -2;
        }
    };
    match handle.cmd_tx.send(cmd) {
        Ok(()) => 0,
        Err(e) => {
            log::warn!("mello_core_send_command: enqueue failed: {e}");
            -3
        }
    }
}

/// Stop and free the core.
///
/// # Safety
/// `handle` must be a pointer returned by `mello_core_create` that has not already
/// been destroyed. Must be called at most once per handle.
#[no_mangle]
pub unsafe extern "C" fn mello_core_destroy(handle: *mut MelloCoreHandle) {
    if handle.is_null() {
        return;
    }
    let mut handle = Box::from_raw(handle);
    let pump = handle.pump.take();
    // Dropping the runtime cancels the client task, which drops the core's
    // `event_tx`, ending the pump thread; dropping `cmd_tx` also breaks the loop.
    drop(handle);
    if let Some(pump) = pump {
        let _ = pump.join();
    }
}

/// Version string for the boot smoke-test (IOS-01 DoD). Caller frees with
/// `mello_core_string_free`.
#[no_mangle]
pub extern "C" fn mello_core_version() -> *mut c_char {
    let version = format!(
        "{} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        mello_core::PROTOCOL_VERSION
    );
    CString::new(version)
        .unwrap_or_else(|_| CString::new("unknown").unwrap())
        .into_raw()
}

/// Free a string returned by this ABI (e.g. `mello_core_version`).
///
/// # Safety
/// `s` must be a pointer returned by this library (or null) and must not be used
/// after this call.
#[no_mangle]
pub unsafe extern "C" fn mello_core_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}
