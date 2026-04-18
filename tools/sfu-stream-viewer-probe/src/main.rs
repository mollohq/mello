use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use mello_core::nakama::WatchStreamResponse;
use mello_core::stream::viewer::{StreamViewer, ViewerAction, ViewerFeedResult};
use mello_core::transport::{SfuConnection, SfuEvent};
use minifb::{Key, Window, WindowOptions};

const WINDOW_W: u32 = 1920;
const WINDOW_H: u32 = 1080;
const CHUNK_HEADER_SIZE: usize = 6; // msg_id(2) + chunk_idx(2) + chunk_count(2)
const MAX_CHUNKS_PER_MESSAGE: u16 = 64;

struct FrameBuffer {
    buf: Vec<u32>,
    width: u32,
    height: u32,
    dirty: bool,
}

struct ChunkAssembly {
    chunk_count: u16,
    chunks_received: u16,
    chunks: Vec<Option<Vec<u8>>>,
}

struct ChunkAssembler {
    pending: HashMap<u16, ChunkAssembly>,
}

impl ChunkAssembler {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    fn feed(&mut self, raw: &[u8]) -> Option<Vec<u8>> {
        if raw.len() < CHUNK_HEADER_SIZE {
            return None;
        }

        let msg_id = u16::from_le_bytes([raw[0], raw[1]]);
        let chunk_idx = u16::from_le_bytes([raw[2], raw[3]]);
        let chunk_count = u16::from_le_bytes([raw[4], raw[5]]);
        let payload = &raw[CHUNK_HEADER_SIZE..];

        if chunk_count == 0 || chunk_count > MAX_CHUNKS_PER_MESSAGE || chunk_idx >= chunk_count {
            return None;
        }

        // Keep only a small rolling window of IDs.
        self.pending.retain(|&id, _| msg_id.wrapping_sub(id) < 64);

        let entry = self.pending.entry(msg_id).or_insert_with(|| ChunkAssembly {
            chunk_count,
            chunks_received: 0,
            chunks: (0..chunk_count).map(|_| None).collect(),
        });

        let idx = chunk_idx as usize;
        if idx < entry.chunks.len() && entry.chunks[idx].is_none() {
            entry.chunks[idx] = Some(payload.to_vec());
            entry.chunks_received += 1;
        }

        if entry.chunks_received == entry.chunk_count {
            let assembly = self.pending.remove(&msg_id).unwrap();
            let total: usize = assembly
                .chunks
                .iter()
                .map(|c| c.as_ref().map_or(0, |v| v.len()))
                .sum();
            let mut out = Vec::with_capacity(total);
            for c in assembly.chunks.into_iter().flatten() {
                out.extend_from_slice(&c);
            }
            Some(out)
        } else {
            None
        }
    }
}

static FRAME: Mutex<Option<FrameBuffer>> = Mutex::new(None);
static FRAMES_DECODED: AtomicU32 = AtomicU32::new(0);
static NATIVE_FRAMES: AtomicU32 = AtomicU32::new(0);
static VIEWER_READY: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn on_decoded_frame(
    _user_data: *mut c_void,
    rgba: *const u8,
    w: u32,
    h: u32,
    _ts: u64,
) {
    if rgba.is_null() || w == 0 || h == 0 {
        return;
    }
    let pixel_count = (w as usize) * (h as usize);
    let src = std::slice::from_raw_parts(rgba, pixel_count * 4);

    let mut pixels = vec![0u32; pixel_count];
    for i in 0..pixel_count {
        let r = src[i * 4] as u32;
        let g = src[i * 4 + 1] as u32;
        let b = src[i * 4 + 2] as u32;
        pixels[i] = (r << 16) | (g << 8) | b;
    }

    if let Ok(mut frame) = FRAME.lock() {
        *frame = Some(FrameBuffer {
            buf: pixels,
            width: w,
            height: h,
            dirty: true,
        });
    }
    FRAMES_DECODED.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn on_native_frame(
    _user_data: *mut c_void,
    _shared_handle: *mut c_void,
    _w: u32,
    _h: u32,
    _format: i32,
    _uv_y_offset: u32,
    _ts: u64,
) {
    NATIVE_FRAMES.fetch_add(1, Ordering::Relaxed);
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    let watch_stream_print = has_flag(&args, "--watch-stream-print");
    let mut endpoint =
        parse_arg_string(&args, "--endpoint").or_else(|| std::env::var("MELLO_SFU_ENDPOINT").ok());
    let mut token =
        parse_arg_string(&args, "--token").or_else(|| std::env::var("MELLO_SFU_TOKEN").ok());
    let session_id = parse_arg_string(&args, "--session")
        .or_else(|| std::env::var("MELLO_SFU_SESSION").ok())
        .unwrap_or_else(|| {
            eprintln!(
                "Missing --session (or MELLO_SFU_SESSION).\n\
                 Example: --session stream_<host>_<id>"
            );
            std::process::exit(1);
        });
    let mut width = parse_arg::<u32>(&args, "--width");
    let mut height = parse_arg::<u32>(&args, "--height");
    let role = parse_arg_string(&args, "--role").unwrap_or_else(|| "viewer".to_string());
    let native_metrics = has_flag(&args, "--native-metrics");

    if watch_stream_print {
        let nakama_http_base = parse_arg_string(&args, "--nakama-http-base")
            .or_else(|| std::env::var("MELLO_NAKAMA_HTTP_BASE").ok())
            .unwrap_or_else(|| {
                eprintln!("Missing --nakama-http-base (or MELLO_NAKAMA_HTTP_BASE)");
                std::process::exit(1);
            });
        let nakama_auth_token = parse_arg_string(&args, "--nakama-auth-token")
            .or_else(|| std::env::var("MELLO_NAKAMA_AUTH_TOKEN").ok())
            .unwrap_or_else(|| {
                eprintln!("Missing --nakama-auth-token (or MELLO_NAKAMA_AUTH_TOKEN)");
                std::process::exit(1);
            });

        let watch_resp =
            request_watch_stream_via_nakama(&nakama_http_base, &nakama_auth_token, &session_id)
                .unwrap_or_else(|e| {
                    eprintln!("watch_stream RPC failed: {}", e);
                    std::process::exit(1);
                });

        println!("watch_stream response:");
        println!("  mode: {}", watch_resp.mode);
        println!(
            "  sfu_endpoint: {}",
            watch_resp.sfu_endpoint.as_deref().unwrap_or("<none>")
        );
        println!(
            "  sfu_token: {}",
            if watch_resp.sfu_token.as_deref().unwrap_or("").is_empty() {
                "<none>"
            } else {
                "<present>"
            }
        );
        println!("  width: {}", watch_resp.width);
        println!("  height: {}", watch_resp.height);
        println!();

        if watch_resp.mode != "sfu" {
            eprintln!(
                "watch_stream returned mode='{}' (expected 'sfu').",
                watch_resp.mode
            );
            std::process::exit(1);
        }
        endpoint = watch_resp.sfu_endpoint.or(endpoint);
        token = watch_resp.sfu_token.or(token);
        if width.is_none() && watch_resp.width > 0 {
            width = Some(watch_resp.width);
        }
        if height.is_none() && watch_resp.height > 0 {
            height = Some(watch_resp.height);
        }
    }

    let endpoint = endpoint.unwrap_or_else(|| {
        eprintln!(
            "Missing --endpoint (or MELLO_SFU_ENDPOINT).\n\
             Example: --endpoint wss://sfu-eu.m3llo.app:8443/ws\n\
             Or use --watch-stream-print with Nakama args to auto-fetch endpoint/token."
        );
        std::process::exit(1);
    });
    let token = token.unwrap_or_else(|| {
        eprintln!(
            "Missing --token (or MELLO_SFU_TOKEN).\n\
             Use the token from watch_stream RPC or use --watch-stream-print to fetch it automatically."
        );
        std::process::exit(1);
    });
    let width = width.unwrap_or(1280);
    let height = height.unwrap_or(720);

    println!("\n=== SFU Stream Viewer Probe ===\n");
    println!("endpoint: {}", endpoint);
    println!("session:  {}", session_id);
    println!("size:     {}x{}", width, height);
    println!("role:     {}", role);
    if native_metrics {
        println!("native metrics: ON");
    }
    let correlation_start = Instant::now();
    let correlation_epoch_ms = unix_time_ms();
    println!("corr_start_unix_ms: {}", correlation_epoch_ms);
    println!();
    log::info!(
        "viewer_probe_start session={} wall_ms={} mono_ms=0 role={} endpoint={} width={} height={}",
        session_id,
        correlation_epoch_ms,
        role,
        endpoint,
        width,
        height
    );

    let ctx = unsafe { mello_sys::mello_init() };
    if ctx.is_null() {
        eprintln!("ERROR: mello_init() failed");
        std::process::exit(1);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    let mut conn = rt
        .block_on(SfuConnection::connect(&endpoint, &token))
        .unwrap_or_else(|e| {
            eprintln!("SFU connect failed: {}", e);
            unsafe {
                mello_sys::mello_destroy(ctx);
            }
            std::process::exit(1);
        });

    let peer_handle = unsafe { SfuConnection::create_peer(ctx) }.unwrap_or_else(|e| {
        eprintln!("SFU peer creation failed: {}", e);
        unsafe {
            mello_sys::mello_destroy(ctx);
        }
        std::process::exit(1);
    });

    rt.block_on(conn.join_stream(peer_handle, &session_id, &role))
        .unwrap_or_else(|e| {
            eprintln!("SFU join_stream failed: {}", e);
            unsafe {
                mello_sys::mello_destroy(ctx);
            }
            std::process::exit(1);
        });

    rt.block_on(conn.wait_for_datachannel_open())
        .unwrap_or_else(|e| {
            eprintln!("SFU datachannel open failed: {}", e);
            unsafe {
                mello_sys::mello_destroy(ctx);
            }
            std::process::exit(1);
        });

    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
    log::info!(
        "viewer_probe_event session={} wall_ms={} mono_ms={} event=viewer_ready server={} region={} rtt_ms={:.1}",
        session_id,
        wall_ms,
        mono_ms,
        conn.server_id(),
        conn.region(),
        conn.rtt_ms()
    );

    let config = mello_sys::MelloStreamConfig {
        width,
        height,
        fps: 60,
        bitrate_kbps: 0,
    };
    let viewer = unsafe {
        mello_sys::mello_stream_start_viewer(
            ctx,
            &config,
            Some(on_decoded_frame),
            std::ptr::null_mut(),
        )
    };
    if viewer.is_null() {
        eprintln!("ERROR: mello_stream_start_viewer() failed");
        unsafe {
            mello_sys::mello_destroy(ctx);
        }
        std::process::exit(1);
    }
    if native_metrics {
        unsafe {
            mello_sys::mello_stream_set_native_frame_callback(
                viewer,
                Some(on_native_frame),
                std::ptr::null_mut(),
            );
            mello_sys::mello_stream_set_native_frame_mirror_rgba(viewer, true);
        }
    }
    VIEWER_READY.store(true, Ordering::Relaxed);

    let mut stream_viewer = StreamViewer::new(mello_core::stream::StreamConfig::default().fec_n);
    let mut chunk_assembler = ChunkAssembler::new();
    let mut got_keyframe = false;

    let mut window = Window::new(
        "SFU Probe - waiting for stream...",
        WINDOW_W as usize,
        WINDOW_H as usize,
        WindowOptions {
            resize: true,
            ..WindowOptions::default()
        },
    )
    .expect("failed to create window");
    window.set_target_fps(120);

    let mut display_buf = vec![0u32; (WINDOW_W * WINDOW_H) as usize];

    let mut last_tick = Instant::now();
    let mut last_decoded = 0u32;
    let mut last_native = 0u32;
    let mut ingress_packets: u64 = 0;
    let mut ingress_bytes: u64 = 0;
    let mut last_ingress_packets: u64 = 0;
    let mut last_ingress_bytes: u64 = 0;
    let mut reassembled_msgs: u64 = 0;
    let mut last_reassembled_msgs: u64 = 0;
    let mut present_calls: u64 = 0;
    let mut present_true: u64 = 0;
    let mut last_present_calls: u64 = 0;
    let mut last_present_true: u64 = 0;

    while window.is_open() && !window.is_key_down(Key::Escape) {
        for ev in conn.poll_events() {
            if let SfuEvent::Disconnected { reason } = ev {
                let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                log::warn!(
                    "viewer_probe_event session={} wall_ms={} mono_ms={} event=disconnected reason={}",
                    session_id,
                    wall_ms,
                    mono_ms,
                    reason
                );
            }
        }

        let raw_packets = conn.poll_recv();
        ingress_packets += raw_packets.len() as u64;
        for raw in &raw_packets {
            ingress_bytes += raw.len() as u64;
            if let Some(full_msg) = chunk_assembler.feed(raw) {
                reassembled_msgs += 1;
                let results = stream_viewer.feed_packet(&full_msg);
                for result in results {
                    match result {
                        ViewerFeedResult::VideoPayload { data, is_keyframe }
                        | ViewerFeedResult::RecoveredVideoPayload { data, is_keyframe } => {
                            if !got_keyframe {
                                if is_keyframe {
                                    got_keyframe = true;
                                    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                                    log::info!(
                                        "viewer_probe_event session={} wall_ms={} mono_ms={} event=first_keyframe",
                                        session_id,
                                        wall_ms,
                                        mono_ms
                                    );
                                } else {
                                    continue;
                                }
                            }
                            let ok = unsafe {
                                mello_sys::mello_stream_feed_packet(
                                    viewer,
                                    data.as_ptr(),
                                    data.len() as i32,
                                    is_keyframe,
                                )
                            };
                            if !ok && is_keyframe {
                                let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                                log::warn!(
                                    "viewer_probe_event session={} wall_ms={} mono_ms={} event=feed_keyframe_failed bytes={}",
                                    session_id,
                                    wall_ms,
                                    mono_ms,
                                    data.len()
                                );
                            }
                        }
                        ViewerFeedResult::AudioPayload(data) => unsafe {
                            let _ = mello_sys::mello_stream_feed_audio_packet(
                                viewer,
                                data.as_ptr(),
                                data.len() as i32,
                            );
                        },
                        ViewerFeedResult::Action(ViewerAction::SendControl(ctrl_data)) => {
                            let _ = conn.send_control(&ctrl_data);
                        }
                        ViewerFeedResult::None => {}
                    }
                }
            }
        }

        present_calls += 1;
        let presented = unsafe { mello_sys::mello_stream_present_frame(viewer) };
        if presented {
            present_true += 1;
        }

        if let Ok(mut frame) = FRAME.lock() {
            if let Some(ref mut fb) = *frame {
                if fb.dirty {
                    display_buf.fill(0);
                    let ox = (WINDOW_W.saturating_sub(fb.width) / 2) as usize;
                    let oy = (WINDOW_H.saturating_sub(fb.height) / 2) as usize;
                    let src_w = fb.width.min(WINDOW_W) as usize;
                    let src_h = fb.height.min(WINDOW_H) as usize;
                    for row in 0..src_h {
                        let dst_start = (oy + row) * WINDOW_W as usize + ox;
                        let src_start = row * fb.width as usize;
                        display_buf[dst_start..dst_start + src_w]
                            .copy_from_slice(&fb.buf[src_start..src_start + src_w]);
                    }
                    let _ = window.update_with_buffer(
                        &display_buf,
                        WINDOW_W as usize,
                        WINDOW_H as usize,
                    );
                    fb.dirty = false;
                } else {
                    drop(frame);
                    window.update();
                }
            } else {
                drop(frame);
                window.update();
            }
        }

        if last_tick.elapsed().as_secs_f32() >= 1.0 {
            let elapsed = last_tick.elapsed().as_secs_f32().max(0.001);
            let decoded_now = FRAMES_DECODED.load(Ordering::Relaxed);
            let native_now = NATIVE_FRAMES.load(Ordering::Relaxed);
            let dec_fps = (decoded_now - last_decoded) as f32 / elapsed;
            let native_fps = (native_now - last_native) as f32 / elapsed;
            let ingress_pps = (ingress_packets - last_ingress_packets) as f32 / elapsed;
            let ingress_kbps =
                ((ingress_bytes - last_ingress_bytes) as f32 * 8.0 / 1000.0) / elapsed;
            let msg_hz = (reassembled_msgs - last_reassembled_msgs) as f32 / elapsed;
            let present_hz = (present_calls - last_present_calls) as f32 / elapsed;
            let present_true_hz = (present_true - last_present_true) as f32 / elapsed;

            let title = format!(
                "SFU Probe | {}x{} dec={:.1}fps native={:.1}fps present={:.1}/{:.1}Hz msgs={:.1}Hz ingress={:.1}pps {:.0}kbps rtt={:.1}ms",
                width,
                height,
                dec_fps,
                native_fps,
                present_true_hz,
                present_hz,
                msg_hz,
                ingress_pps,
                ingress_kbps,
                conn.rtt_ms(),
            );
            window.set_title(&title);

            let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
            log::info!(
                "viewer_probe_tick session={} wall_ms={} mono_ms={} dec_fps={:.1} native_fps={:.1} present_true_hz={:.1} present_hz={:.1} msg_hz={:.1} ingress_pps={:.1} ingress_kbps={:.0} rtt_ms={:.1}",
                session_id,
                wall_ms,
                mono_ms,
                dec_fps,
                native_fps,
                present_true_hz,
                present_hz,
                msg_hz,
                ingress_pps,
                ingress_kbps,
                conn.rtt_ms()
            );

            last_tick = Instant::now();
            last_decoded = decoded_now;
            last_native = native_now;
            last_ingress_packets = ingress_packets;
            last_ingress_bytes = ingress_bytes;
            last_reassembled_msgs = reassembled_msgs;
            last_present_calls = present_calls;
            last_present_true = present_true;
        }
    }

    rt.block_on(conn.leave());
    unsafe {
        mello_sys::mello_stream_stop_viewer(viewer);
        mello_sys::mello_destroy(ctx);
    }
}

fn parse_arg<T: std::str::FromStr>(args: &[String], flag: &str) -> Option<T> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

fn parse_arg_string(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|v| v.to_string())
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn correlation_stamp(start: Instant) -> (u128, u128) {
    (unix_time_ms(), start.elapsed().as_millis())
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn request_watch_stream_via_nakama(
    http_base: &str,
    auth_token: &str,
    session_id: &str,
) -> Result<WatchStreamResponse, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    rt.block_on(async move {
        let url = format!("{}/v2/rpc/watch_stream", http_base.trim_end_matches('/'));
        let payload = serde_json::json!({
            "session_id": session_id,
        });
        // Nakama RPC HTTP expects the payload to be a JSON string.
        let body = serde_json::Value::String(payload.to_string());

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .bearer_auth(auth_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_text = resp.text().await.unwrap_or_default();
            return Err(format!("HTTP {}: {}", status, err_text));
        }

        let rpc = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string())?;
        let payload = rpc
            .get("payload")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing RPC payload".to_string())?;

        serde_json::from_str::<WatchStreamResponse>(payload).map_err(|e| e.to_string())
    })
}
