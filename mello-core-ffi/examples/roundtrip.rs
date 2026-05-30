//! Host proof of the FFI spine: Swift -> Command -> core -> Event -> Swift,
//! driven entirely through the C-ABI exactly as the iOS app will, but against
//! the local dev Nakama. No libmello path is exercised (auth only), so this
//! validates config marshalling, the adjacently-tagged Command/Event JSON
//! shapes, and the callback threading independently of the libmello-iOS port.
//!
//! Run with a local dev Nakama up (see `client-dev.sh`):
//!   cargo run -p mello-core-ffi --example roundtrip
//!   cargo run -p mello-core-ffi --example roundtrip -- <device_id>

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use mello_core::Config;
use mello_core_ffi::{
    mello_core_create, mello_core_destroy, mello_core_send_command, mello_core_string_free,
    mello_core_version, MelloEventCallback,
};

/// Invoked by the FFI event-pump thread. `user_data` points at the `Sender`.
extern "C" fn on_event(event_json: *const c_char, user_data: *mut c_void) {
    let json = unsafe { CStr::from_ptr(event_json) }
        .to_string_lossy()
        .into_owned();
    let tx = unsafe { &*(user_data as *const Sender<String>) };
    let _ = tx.send(json);
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let device_id = std::env::args().nth(1).unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("ios-ffi-probe-{nanos}")
    });

    // Version smoke-test (IOS-01 DoD).
    let v = mello_core_version();
    println!("mello_core_version: {}", unsafe {
        CStr::from_ptr(v).to_string_lossy()
    });
    unsafe { mello_core_string_free(v) };

    // Local dev Nakama config, serialized the way Swift will send it.
    let config_json = serde_json::to_string(&Config::development()).unwrap();
    println!("config: {config_json}");

    let (tx, rx) = mpsc::channel::<String>();
    // Keep the Sender alive for the whole handle lifetime; hand its address to the
    // callback as `user_data` (mirrors Swift's `Unmanaged<MelloCore>` pattern).
    let tx_box = Box::new(tx);
    let cb = MelloEventCallback {
        callback: on_event,
        user_data: &*tx_box as *const Sender<String> as *mut c_void,
    };

    let config_c = CString::new(config_json).unwrap();
    let handle = unsafe { mello_core_create(config_c.as_ptr(), cb) };
    assert!(!handle.is_null(), "mello_core_create returned null");

    // Hand-written to mirror exactly what the Swift side will marshal.
    let command_json = format!(r#"{{"type":"DeviceAuth","data":{{"device_id":"{device_id}"}}}}"#);
    println!("-> send: {command_json}");
    let command_c = CString::new(command_json).unwrap();
    let rc = unsafe { mello_core_send_command(handle, command_c.as_ptr()) };
    assert_eq!(rc, 0, "send_command failed: {rc}");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut outcome = None;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(json) => {
                println!("<- event: {json}");
                if json.contains("\"DeviceAuthed\"") || json.contains("\"LoginFailed\"") {
                    outcome = Some(json);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    unsafe { mello_core_destroy(handle) };
    drop(tx_box);

    match outcome {
        Some(json) if json.contains("\"DeviceAuthed\"") => {
            println!("\nROUND TRIP OK: DeviceAuthed received through the C-ABI.");
        }
        Some(json) => {
            eprintln!("\nROUND TRIP reached core but auth failed: {json}");
            std::process::exit(1);
        }
        None => {
            eprintln!("\nNO EVENT within timeout - is local dev Nakama running (client-dev.sh)?");
            std::process::exit(2);
        }
    }
}
