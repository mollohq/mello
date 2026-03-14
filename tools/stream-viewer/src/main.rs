use std::ffi::c_void;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use minifb::{Key, Window, WindowOptions};

const DEFAULT_PORT: u16 = 9800;
const HEADER_CONFIG: u8 = 0x00;
const HEADER_VIDEO: u8 = 0x01;
const HEADER_KEYFRAME: u8 = 0x02;

const DEFAULT_W: u32 = 1920;
const DEFAULT_H: u32 = 1080;

struct FrameBuffer {
    buf: Vec<u32>,
    width: u32,
    height: u32,
    dirty: bool,
}

static FRAME: Mutex<Option<FrameBuffer>> = Mutex::new(None);
static FRAMES_DECODED: AtomicU32 = AtomicU32::new(0);
static VIEWER_READY: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn on_decoded_frame(
    _user_data: *mut c_void,
    rgba: *const u8,
    w: u32,
    h: u32,
    _ts: u64,
) {
    let pixel_count = (w * h) as usize;
    let src = std::slice::from_raw_parts(rgba, pixel_count * 4);

    // Convert RGBA -> 0xAARRGGBB (minifb native format)
    let mut pixels = vec![0u32; pixel_count];
    for i in 0..pixel_count {
        let r = src[i * 4] as u32;
        let g = src[i * 4 + 1] as u32;
        let b = src[i * 4 + 2] as u32;
        pixels[i] = (r << 16) | (g << 8) | b;
    }

    let mut frame = FRAME.lock().unwrap();
    *frame = Some(FrameBuffer {
        buf: pixels,
        width: w,
        height: h,
        dirty: true,
    });

    FRAMES_DECODED.fetch_add(1, Ordering::Relaxed);
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    let port: u16 = parse_arg(&args, "--port").unwrap_or(DEFAULT_PORT);
    let initial_w: u32 = parse_arg(&args, "--width").unwrap_or(DEFAULT_W);
    let initial_h: u32 = parse_arg(&args, "--height").unwrap_or(DEFAULT_H);

    let ctx = unsafe { mello_sys::mello_init() };
    if ctx.is_null() {
        eprintln!("ERROR: mello_init() failed");
        std::process::exit(1);
    }

    println!("\n=== Mello Stream Viewer ===\n");
    println!("Listening on UDP 0.0.0.0:{}...\n", port);

    let socket = UdpSocket::bind(format!("0.0.0.0:{}", port)).expect("Failed to bind UDP socket");
    socket
        .set_nonblocking(false)
        .expect("Failed to set socket blocking");
    socket
        .set_read_timeout(Some(std::time::Duration::from_millis(50)))
        .ok();

    // Wait for first packet (blocking)
    println!("Waiting for stream from host...");

    let mut recv_buf = [0u8; 512 * 1024]; // 512KB max packet
    let mut viewer: *mut mello_sys::MelloStreamView = std::ptr::null_mut();
    let mut got_keyframe = false;
    let mut frame_w = initial_w;
    let mut frame_h = initial_h;

    // Create window
    let mut window = Window::new(
        "Mello Viewer — waiting for stream...",
        frame_w as usize,
        frame_h as usize,
        WindowOptions {
            resize: true,
            ..WindowOptions::default()
        },
    )
    .expect("Failed to create window");

    window.set_target_fps(120);

    let start_time = Instant::now();
    let mut last_fps_check = Instant::now();
    let mut last_frame_count = 0u32;

    while window.is_open() && !window.is_key_down(Key::Escape) {
        // Receive UDP packets
        match socket.recv_from(&mut recv_buf) {
            Ok((n, _addr)) if n >= 2 => {
                let header = recv_buf[0];
                let payload = &recv_buf[1..n];

                match header {
                    HEADER_CONFIG => {
                        if payload.len() >= 5 {
                            let w = u16::from_le_bytes([payload[0], payload[1]]) as u32;
                            let h = u16::from_le_bytes([payload[2], payload[3]]) as u32;
                            if w > 0 && h > 0 {
                                frame_w = w;
                                frame_h = h;
                            }
                            log::info!("Config: {}x{} fps={}", frame_w, frame_h, payload[4]);
                        }
                    }
                    HEADER_VIDEO | HEADER_KEYFRAME => {
                        let is_keyframe = header == HEADER_KEYFRAME;

                        // Initialize viewer on first keyframe
                        if !got_keyframe && is_keyframe {
                            got_keyframe = true;
                            println!("First keyframe received, starting decode...");

                            let config = mello_sys::MelloStreamConfig {
                                width: frame_w,
                                height: frame_h,
                                fps: 60,
                                bitrate_kbps: 0,
                            };

                            viewer = unsafe {
                                mello_sys::mello_stream_start_viewer(
                                    ctx,
                                    &config,
                                    Some(on_decoded_frame),
                                    std::ptr::null_mut(),
                                )
                            };
                            if viewer.is_null() {
                                eprintln!("ERROR: mello_stream_start_viewer() failed");
                                break;
                            }
                            VIEWER_READY.store(true, Ordering::Relaxed);
                        }

                        if !viewer.is_null() {
                            unsafe {
                                mello_sys::mello_stream_feed_packet(
                                    viewer,
                                    payload.as_ptr(),
                                    payload.len() as i32,
                                    is_keyframe,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {} // timeout or error — continue to update window
        }

        // Update window with latest decoded frame
        let mut frame = FRAME.lock().unwrap();
        if let Some(ref mut fb) = *frame {
            if fb.dirty {
                // Resize window if frame dimensions changed
                if fb.width != frame_w || fb.height != frame_h {
                    frame_w = fb.width;
                    frame_h = fb.height;
                }

                window
                    .update_with_buffer(&fb.buf, fb.width as usize, fb.height as usize)
                    .unwrap_or_else(|e| {
                        log::warn!("Window update failed: {}", e);
                    });
                fb.dirty = false;
            } else {
                drop(frame);
                window.update();
            }
        } else {
            drop(frame);
            window.update();
        }

        // Update title with stats every second
        let now = Instant::now();
        if now.duration_since(last_fps_check).as_millis() >= 1000 {
            let total = FRAMES_DECODED.load(Ordering::Relaxed);
            let fps = total - last_frame_count;
            let elapsed = start_time.elapsed().as_secs();

            let title = if got_keyframe {
                format!(
                    "Mello Viewer — {}x{} @ {}fps | {}s",
                    frame_w, frame_h, fps, elapsed
                )
            } else {
                "Mello Viewer — waiting for keyframe...".to_string()
            };
            window.set_title(&title);

            last_frame_count = total;
            last_fps_check = now;
        }
    }

    println!("\nShutting down...");
    if !viewer.is_null() {
        unsafe { mello_sys::mello_stream_stop_viewer(viewer) };
    }
    unsafe { mello_sys::mello_destroy(ctx) };

    let total = FRAMES_DECODED.load(Ordering::Relaxed);
    println!(
        "Total frames decoded: {} in {:.1}s",
        total,
        start_time.elapsed().as_secs_f64()
    );
    println!("Done.");
}

fn parse_arg<T: std::str::FromStr>(args: &[String], flag: &str) -> Option<T> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}
