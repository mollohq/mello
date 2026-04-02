//! End-to-end loopback test for the video pipeline via FFI.
//! Captures the primary monitor, encodes, feeds packets back to a viewer, asserts frames decoded.
//! Skips automatically if no hardware encoder is available.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

struct Packet {
    data: Vec<u8>,
    is_keyframe: bool,
}

static PACKETS: Mutex<Vec<Packet>> = Mutex::new(Vec::new());
static FRAMES_DECODED: AtomicU32 = AtomicU32::new(0);
static LAST_W: AtomicU32 = AtomicU32::new(0);
static LAST_H: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn on_video_packet(
    _user_data: *mut c_void,
    data: *const u8,
    size: i32,
    is_keyframe: bool,
    _ts: u64,
) {
    let slice = std::slice::from_raw_parts(data, size as usize);
    PACKETS.lock().unwrap().push(Packet {
        data: slice.to_vec(),
        is_keyframe,
    });
}

unsafe extern "C" fn on_decoded_frame(
    _user_data: *mut c_void,
    _rgba: *const u8,
    w: u32,
    h: u32,
    _ts: u64,
) {
    FRAMES_DECODED.fetch_add(1, Ordering::Relaxed);
    LAST_W.store(w, Ordering::Relaxed);
    LAST_H.store(h, Ordering::Relaxed);
}

fn with_ctx(f: impl FnOnce(*mut mello_sys::MelloContext)) {
    let ctx = unsafe { mello_sys::mello_init() };
    assert!(!ctx.is_null(), "mello_init failed");
    f(ctx);
    unsafe {
        mello_sys::mello_destroy(ctx);
    }
}

#[test]
fn encoder_available() {
    with_ctx(|ctx| {
        let avail = unsafe { mello_sys::mello_encoder_available(ctx) };
        if !avail {
            eprintln!("SKIP: No hardware encoder available on this machine");
        }
        // Don't assert — just log. The loopback test will skip too.
    });
}

#[test]
fn host_to_viewer_loopback() {
    if cfg!(target_os = "macos") && std::env::var("CI").is_ok() {
        eprintln!(
            "SKIP: Loopback test disabled on macOS CI (ScreenCaptureKit blocks without TCC screen recording permission)"
        );
        return;
    }

    // Reset globals
    PACKETS.lock().unwrap().clear();
    FRAMES_DECODED.store(0, Ordering::Relaxed);
    LAST_W.store(0, Ordering::Relaxed);
    LAST_H.store(0, Ordering::Relaxed);

    with_ctx(|ctx| unsafe {
        if !mello_sys::mello_encoder_available(ctx) {
            eprintln!("SKIP: No hardware encoder — skipping loopback test");
            return;
        }

        let source = mello_sys::MelloCaptureSource {
            mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
            monitor_index: 0,
            hwnd: std::ptr::null_mut(),
            pid: 0,
        };

        let config = mello_sys::MelloStreamConfig {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 5000,
        };

        // Phase 1: Host — capture + encode for 2 seconds
        let host = mello_sys::mello_stream_start_host(
            ctx,
            &source,
            &config,
            Some(on_video_packet),
            std::ptr::null_mut(),
        );
        if host.is_null() {
            eprintln!("SKIP: mello_stream_start_host failed (no desktop session?)");
            return;
        }

        std::thread::sleep(Duration::from_secs(2));

        mello_sys::mello_stream_stop_host(host);

        let packets = PACKETS.lock().unwrap();
        assert!(
            !packets.is_empty(),
            "No packets produced in 2 seconds of capture"
        );
        let has_keyframe = packets.iter().any(|p| p.is_keyframe);
        assert!(has_keyframe, "No keyframe in captured packets");

        eprintln!(
            "Host produced {} packets ({} keyframes)",
            packets.len(),
            packets.iter().filter(|p| p.is_keyframe).count()
        );

        // Phase 2: Viewer — decode
        let view = mello_sys::mello_stream_start_viewer(
            ctx,
            &config,
            Some(on_decoded_frame),
            std::ptr::null_mut(),
        );
        assert!(!view.is_null(), "mello_stream_start_viewer failed");

        let mut seen_keyframe = false;
        for p in packets.iter() {
            if !seen_keyframe {
                if p.is_keyframe {
                    seen_keyframe = true;
                } else {
                    continue;
                }
            }
            mello_sys::mello_stream_feed_packet(
                view,
                p.data.as_ptr(),
                p.data.len() as i32,
                p.is_keyframe,
            );
        }

        // Trigger staging readback + RGBA conversion + callback
        mello_sys::mello_stream_present_frame(view);

        std::thread::sleep(Duration::from_millis(200));

        mello_sys::mello_stream_stop_viewer(view);

        let decoded = FRAMES_DECODED.load(Ordering::Relaxed);
        let w = LAST_W.load(Ordering::Relaxed);
        let h = LAST_H.load(Ordering::Relaxed);

        eprintln!("Viewer decoded {} frames ({}x{})", decoded, w, h);
        assert!(decoded > 0, "No frames decoded");
        assert_eq!(w, config.width, "Decoded width mismatch");
        assert_eq!(h, config.height, "Decoded height mismatch");
    });
}
