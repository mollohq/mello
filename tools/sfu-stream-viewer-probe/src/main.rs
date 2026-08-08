use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mello_core::client::feed_viewer_audio_packet;
use mello_core::nakama::WatchStreamResponse;
use mello_core::transport::{SfuConnection, SfuEvent, StreamPeerRole};
use minifb::{Key, Window, WindowOptions};

const DEFAULT_WINDOW_W: u32 = 1280;
const DEFAULT_WINDOW_H: u32 = 720;
const MAX_AU_POLLS_PER_TICK: usize = 32;
const VIEWER_AU_RECV_BUF_INITIAL: usize = 256 * 1024;
const DEFAULT_RECEIVE_BITRATE_KBPS: u32 = 6000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeMode {
    CpuWindow,
    NativeMetricsHeadless,
}

impl ProbeMode {
    fn from_args(args: &[String]) -> Self {
        if has_flag(args, "--native-metrics") {
            Self::NativeMetricsHeadless
        } else {
            Self::CpuWindow
        }
    }

    fn is_native_metrics(self) -> bool {
        self == Self::NativeMetricsHeadless
    }
}

struct FrameBuffer {
    buf: Vec<u32>,
    width: u32,
    height: u32,
    dirty: bool,
}

static FRAME: Mutex<Option<FrameBuffer>> = Mutex::new(None);
static FRAMES_DECODED: AtomicU32 = AtomicU32::new(0);
static NATIVE_FRAMES: AtomicU32 = AtomicU32::new(0);
static VIEWER_READY: AtomicBool = AtomicBool::new(false);
static LAST_FRAME_WALL_MS: AtomicU64 = AtomicU64::new(0);

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
    LAST_FRAME_WALL_MS.store(unix_time_ms() as u64, Ordering::Relaxed);
}

unsafe extern "C" fn on_native_frame(
    _user_data: *mut c_void,
    _shared_handle: *mut c_void,
    _w: u32,
    _h: u32,
    _format: mello_sys::MelloNativeFrameFormat,
    _uv_y_offset: u32,
    _ts: u64,
) {
    NATIVE_FRAMES.fetch_add(1, Ordering::Relaxed);
    LAST_FRAME_WALL_MS.store(unix_time_ms() as u64, Ordering::Relaxed);
}

fn try_set_receive_target(
    conn: &SfuConnection,
    receive_target_bps: u32,
    receive_target_set: &mut bool,
) {
    if *receive_target_set || receive_target_bps == 0 {
        return;
    }
    if !conn.is_video_track_open() {
        return;
    }
    match conn.set_video_receive_target(receive_target_bps) {
        Ok(()) => {
            *receive_target_set = true;
            log::info!(
                "SFU viewer RTP receive target set to {} bps",
                receive_target_bps
            );
        }
        Err(e) => log::warn!("Failed to set SFU viewer receive target: {}", e),
    }
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
    let mode = ProbeMode::from_args(&args);
    let native_metrics = mode.is_native_metrics();
    let hold_sec = parse_arg::<u64>(&args, "--hold-sec");
    if hold_sec.is_some() && !native_metrics {
        eprintln!("--hold-sec requires --native-metrics");
        std::process::exit(1);
    }
    let mut receive_bitrate_kbps = DEFAULT_RECEIVE_BITRATE_KBPS;

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
        println!("  bitrate_kbps: {}", watch_resp.bitrate_kbps);
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
        if watch_resp.bitrate_kbps > 0 {
            receive_bitrate_kbps = watch_resp.bitrate_kbps;
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
    let receive_target_bps = receive_bitrate_kbps.saturating_mul(1_000);

    println!("\n=== SFU Stream Viewer Probe ===\n");
    println!("endpoint: {}", endpoint);
    println!("session:  {}", session_id);
    println!("size:     {}x{}", width, height);
    println!("role:     {}", role);
    println!("receive_target_kbps: {}", receive_bitrate_kbps);
    if native_metrics {
        if let Some(sec) = hold_sec {
            println!("mode:     native metrics (headless; hold {}s)", sec);
        } else {
            println!("mode:     native metrics (headless; press Ctrl+C to exit)");
        }
    } else {
        println!("mode:     CPU RGBA window (close it or press Escape to exit)");
    }
    let correlation_start = Instant::now();
    let run_until = hold_sec.map(|sec| correlation_start + Duration::from_secs(sec));
    let correlation_epoch_ms = unix_time_ms();
    println!("corr_start_unix_ms: {}", correlation_epoch_ms);
    println!();
    log::info!(
        "viewer_probe_start session={} wall_ms={} mono_ms=0 role={} endpoint={} width={} height={} receive_target_kbps={}",
        session_id,
        correlation_epoch_ms,
        role,
        endpoint,
        width,
        height,
        receive_bitrate_kbps
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
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let _shutdown_task = if native_metrics {
        let shutdown_requested = shutdown_requested.clone();
        Some(rt.spawn(async move {
            if let Err(err) = tokio::signal::ctrl_c().await {
                eprintln!("Failed to listen for Ctrl+C: {}", err);
            }
            shutdown_requested.store(true, Ordering::Relaxed);
        }))
    } else {
        None
    };

    let mut conn = rt
        .block_on(SfuConnection::connect(&endpoint, &token))
        .unwrap_or_else(|e| {
            eprintln!("SFU connect failed: {}", e);
            unsafe {
                mello_sys::mello_destroy(ctx);
            }
            std::process::exit(1);
        });

    let peer_handle = unsafe { SfuConnection::create_stream_peer(ctx, StreamPeerRole::Viewer) }
        .unwrap_or_else(|e| {
            eprintln!("SFU stream viewer peer creation failed: {}", e);
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
        if native_metrics {
            mello_sys::mello_stream_start_viewer(ctx, &config, None, std::ptr::null_mut())
        } else {
            mello_sys::mello_stream_start_viewer(
                ctx,
                &config,
                Some(on_decoded_frame),
                std::ptr::null_mut(),
            )
        }
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
        }
    }
    VIEWER_READY.store(true, Ordering::Relaxed);

    let mut au_recv_buf = Vec::with_capacity(VIEWER_AU_RECV_BUF_INITIAL);
    let mut receive_target_set = false;
    let mut logged_first_keyframe = false;

    let mut window = if native_metrics {
        None
    } else {
        let mut window = Window::new(
            "SFU Probe - waiting for stream...",
            DEFAULT_WINDOW_W as usize,
            DEFAULT_WINDOW_H as usize,
            WindowOptions {
                resize: true,
                ..WindowOptions::default()
            },
        )
        .expect("failed to create window");
        window.set_target_fps(120);
        Some(window)
    };

    let mut display_buf = if window.is_some() {
        vec![0u32; (DEFAULT_WINDOW_W * DEFAULT_WINDOW_H) as usize]
    } else {
        Vec::new()
    };

    let mut last_tick = Instant::now();
    let mut last_decoded = 0u32;
    let mut last_native = 0u32;
    let mut au_received: u64 = 0;
    let mut au_fed: u64 = 0;
    let mut au_feed_failures: u64 = 0;
    let mut au_poll_errors: u64 = 0;
    let mut au_buffer_grows: u64 = 0;
    let mut present_calls: u64 = 0;
    let mut present_true: u64 = 0;
    let mut last_au_received: u64 = 0;
    let mut last_au_fed: u64 = 0;
    let mut last_au_feed_failures: u64 = 0;
    let mut last_present_calls: u64 = 0;
    let mut last_present_true: u64 = 0;
    let mut last_rx_ingress_packets: u64 = 0;
    let mut last_rx_ingress_bytes: u64 = 0;
    let mut last_rx_missing: u64 = 0;
    let mut last_rx_repaired: u64 = 0;
    let mut last_rx_nacks: u64 = 0;
    let mut last_rx_pli: u64 = 0;
    let mut audio_received: u64 = 0;
    let mut audio_fed: u64 = 0;
    let mut audio_feed_failures: u64 = 0;
    let mut last_audio_received: u64 = 0;
    let mut last_audio_fed: u64 = 0;
    let mut last_audio_feed_failures: u64 = 0;
    let mut logged_first_audio = false;

    loop {
        if shutdown_requested.load(Ordering::Relaxed) {
            break;
        }
        let ice_state = conn.ice_connection_state();
        if matches!(ice_state, 3..=5) {
            let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
            log::warn!(
                "viewer_probe_event session={} wall_ms={} mono_ms={} event=ice_lost ice_state={}",
                session_id,
                wall_ms,
                mono_ms,
                ice_state
            );
            break;
        }
        if run_until.is_some_and(|deadline| Instant::now() >= deadline) {
            log::info!(
                "viewer_probe_event session={} event=hold_complete hold_sec={}",
                session_id,
                hold_sec.unwrap_or(0)
            );
            break;
        }
        if let Some(window) = window.as_ref() {
            if !window.is_open() || window.is_key_down(Key::Escape) {
                break;
            }
        }

        try_set_receive_target(&conn, receive_target_bps, &mut receive_target_set);

        for _ in 0..MAX_AU_POLLS_PER_TICK {
            let before_len = au_recv_buf.len();
            let au = match conn.poll_received_access_unit(&mut au_recv_buf) {
                Ok(Some(au)) => au,
                Ok(None) => break,
                Err(e) => {
                    au_poll_errors = au_poll_errors.saturating_add(1);
                    log::warn!("SFU viewer AU poll failed: {}", e);
                    break;
                }
            };
            au_received = au_received.saturating_add(1);
            if au_recv_buf.len() > before_len {
                au_buffer_grows = au_buffer_grows.saturating_add(1);
            }

            let ok = unsafe {
                mello_sys::mello_stream_feed_packet(
                    viewer,
                    au_recv_buf.as_ptr(),
                    i32::try_from(au_recv_buf.len()).unwrap_or(i32::MAX),
                    au.is_idr,
                )
            };
            if ok {
                au_fed = au_fed.saturating_add(1);
                if !logged_first_keyframe && au.is_idr {
                    logged_first_keyframe = true;
                    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                    log::info!(
                        "viewer_probe_event session={} wall_ms={} mono_ms={} event=first_keyframe",
                        session_id,
                        wall_ms,
                        mono_ms
                    );
                }
            } else {
                au_feed_failures = au_feed_failures.saturating_add(1);
                if au.is_idr {
                    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                    log::warn!(
                        "viewer_probe_event session={} wall_ms={} mono_ms={} event=feed_keyframe_failed bytes={}",
                        session_id,
                        wall_ms,
                        mono_ms,
                        au_recv_buf.len()
                    );
                }
            }
        }

        let mut transport_lost = false;
        for ev in conn.poll_events() {
            match ev {
                SfuEvent::Disconnected { reason } => {
                    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                    log::warn!(
                        "viewer_probe_event session={} wall_ms={} mono_ms={} event=disconnected reason={}",
                        session_id,
                        wall_ms,
                        mono_ms,
                        reason
                    );
                    transport_lost = true;
                }
                SfuEvent::MediaPacket { .. } => {}
                SfuEvent::AudioTrackData { data, .. } => {
                    audio_received = audio_received.saturating_add(1);
                    // SAFETY: `viewer` is the handle returned by the viewer
                    // start call above and stays valid for this loop.
                    if unsafe { feed_viewer_audio_packet(viewer, &data) } {
                        audio_fed = audio_fed.saturating_add(1);
                        if !logged_first_audio {
                            logged_first_audio = true;
                            let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                            log::info!(
                                "viewer_probe_event session={} wall_ms={} mono_ms={} event=first_audio_packet bytes={}",
                                session_id,
                                wall_ms,
                                mono_ms,
                                data.len()
                            );
                        }
                    } else {
                        audio_feed_failures = audio_feed_failures.saturating_add(1);
                    }
                }
                _ => {}
            }
        }
        if transport_lost {
            break;
        }

        present_calls += 1;
        let presented = unsafe { mello_sys::mello_stream_present_frame(viewer) };
        if presented {
            present_true += 1;
        }

        if let Some(window) = window.as_mut() {
            if let Ok(mut frame) = FRAME.lock() {
                if let Some(ref mut fb) = *frame {
                    if fb.dirty {
                        let (win_w, win_h) = window.get_size();
                        let pixel_count = win_w.saturating_mul(win_h);
                        if display_buf.len() != pixel_count {
                            display_buf.resize(pixel_count, 0);
                        }
                        blit_scaled_fit(
                            &fb.buf,
                            fb.width,
                            fb.height,
                            &mut display_buf,
                            win_w as u32,
                            win_h as u32,
                        );
                        let _ = window.update_with_buffer(&display_buf, win_w, win_h);
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
        }

        if last_tick.elapsed().as_secs_f32() >= 1.0 {
            let elapsed = last_tick.elapsed().as_secs_f32().max(0.001);
            let native_now = NATIVE_FRAMES.load(Ordering::Relaxed);
            let decoded_now = FRAMES_DECODED.load(Ordering::Relaxed);
            let dec_fps = (decoded_now - last_decoded) as f32 / elapsed;
            let native_fps = (native_now - last_native) as f32 / elapsed;
            let au_received_hz = (au_received - last_au_received) as f32 / elapsed;
            let au_fed_hz = (au_fed - last_au_fed) as f32 / elapsed;
            let au_feed_fail_hz = (au_feed_failures - last_au_feed_failures) as f32 / elapsed;
            let present_hz = (present_calls - last_present_calls) as f32 / elapsed;
            let present_fps = (present_true - last_present_true) as f32 / elapsed;
            let audio_received_hz = (audio_received - last_audio_received) as f32 / elapsed;
            let audio_fed_hz = (audio_fed - last_audio_fed) as f32 / elapsed;
            let audio_feed_fail_hz =
                (audio_feed_failures - last_audio_feed_failures) as f32 / elapsed;

            let decode_queue_depth =
                unsafe { mello_sys::mello_stream_viewer_decode_queue_depth(viewer) };
            let now_wall_ms = unix_time_ms() as u64;
            let last_frame_wall_ms = LAST_FRAME_WALL_MS.load(Ordering::Relaxed);
            let decode_stall_ms = if last_frame_wall_ms > 0 && now_wall_ms >= last_frame_wall_ms {
                now_wall_ms - last_frame_wall_ms
            } else {
                0
            };

            let stats = conn.video_stats().ok();
            let (
                rx_ingress_packets,
                rx_ingress_bytes,
                rx_missing,
                rx_repaired,
                rx_nacks,
                rx_pli,
                rx_jitter,
                rx_gated,
                rx_buffered_aus,
                rx_receive_target_bps,
                rx_fec_recovered,
                rx_fec_unrecoverable,
            ) = if let Some(ref s) = stats {
                (
                    s.rx_ingress_packets,
                    s.rx_ingress_bytes,
                    s.rx_core_missing_sequences_detected,
                    s.rx_core_repaired_packets,
                    s.rx_core_nacks,
                    s.rx_core_pli_requests,
                    s.rx_core_interarrival_jitter,
                    s.rx_core_gated,
                    s.rx_core_buffered_access_units,
                    s.rx_receive_target_bps,
                    s.rx_fec_recovered,
                    s.rx_fec_unrecoverable,
                )
            } else {
                (0, 0, 0, 0, 0, 0, 0, 0, 0, receive_target_bps, 0, 0)
            };

            let ingress_pps =
                (rx_ingress_packets.saturating_sub(last_rx_ingress_packets)) as f32 / elapsed;
            let ingress_kbps =
                ((rx_ingress_bytes.saturating_sub(last_rx_ingress_bytes)) as f32 * 8.0 / 1000.0)
                    / elapsed;
            let missing_hz = (rx_missing.saturating_sub(last_rx_missing)) as f32 / elapsed;
            let repaired_hz = (rx_repaired.saturating_sub(last_rx_repaired)) as f32 / elapsed;
            let nacks_hz = (rx_nacks.saturating_sub(last_rx_nacks)) as f32 / elapsed;
            let pli_hz = (rx_pli.saturating_sub(last_rx_pli)) as f32 / elapsed;

            let title = format!(
                "SFU Probe | {}x{} dec={:.1}fps native={:.1}fps au={:.1}/{:.1}Hz audio={:.1}/{:.1}Hz queue={} present={:.1}Hz ingress={:.1}pps {:.0}kbps fec_rec={} rtt={:.1}ms gated={}",
                width,
                height,
                dec_fps,
                native_fps,
                au_fed_hz,
                au_received_hz,
                audio_fed_hz,
                audio_received_hz,
                decode_queue_depth,
                present_fps,
                ingress_pps,
                ingress_kbps,
                rx_fec_recovered,
                conn.rtt_ms(),
                rx_gated,
            );
            if let Some(window) = window.as_mut() {
                window.set_title(&title);
            }

            let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
            log::info!(
                "viewer_probe_tick session={} wall_ms={} mono_ms={} au_received_hz={:.1} au_fed_hz={:.1} au_feed_fail_hz={:.1} au_poll_errors={} au_buffer_grows={} audio_received_hz={:.1} audio_fed_hz={:.1} audio_feed_fail_hz={:.1} rx_audio_packets={} rx_audio_fed={} rx_audio_feed_fail={} dec_fps={:.1} native_fps={:.1} present_fps={:.1} present_hz={:.1} decode_queue_depth={} decode_stall_ms={} rtt_ms={:.1} rx_ingress_packets={} rx_ingress_bytes={} rx_ingress_pps={:.1} rx_ingress_kbps={:.0} rx_missing_hz={:.1} rx_repaired_hz={:.1} rx_nacks_hz={:.1} rx_pli_hz={:.1} rx_fec_recovered={} rx_fec_unrecoverable={} rx_jitter={} rx_gated={} rx_buffered_aus={} rx_receive_target_bps={}",
                session_id,
                wall_ms,
                mono_ms,
                au_received_hz,
                au_fed_hz,
                au_feed_fail_hz,
                au_poll_errors,
                au_buffer_grows,
                audio_received_hz,
                audio_fed_hz,
                audio_feed_fail_hz,
                audio_received,
                audio_fed,
                audio_feed_failures,
                dec_fps,
                native_fps,
                present_fps,
                present_hz,
                decode_queue_depth,
                decode_stall_ms,
                conn.rtt_ms(),
                rx_ingress_packets,
                rx_ingress_bytes,
                ingress_pps,
                ingress_kbps,
                missing_hz,
                repaired_hz,
                nacks_hz,
                pli_hz,
                rx_fec_recovered,
                rx_fec_unrecoverable,
                rx_jitter,
                rx_gated,
                rx_buffered_aus,
                rx_receive_target_bps,
            );

            if let Some(ref s) = stats {
                log::info!(
                    "viewer_probe_native_rtp session={} wall_ms={} mono_ms={} rx_complete={} rx_emitted={} rx_incomplete={} gate_dropped={} nacks={} pli={} jitter={} buffered_aus={} ingress_packets={} ingress_bytes={} missing={} repaired={} fec_recovered={} fec_unrecoverable={} gate_state={} receive_target_bps={}",
                    session_id,
                    wall_ms,
                    mono_ms,
                    s.rx_core_complete_access_units,
                    s.rx_core_emitted_access_units,
                    s.rx_core_incomplete_access_units,
                    s.rx_core_gate_dropped_access_units,
                    s.rx_core_nacks,
                    s.rx_core_pli_requests,
                    s.rx_core_interarrival_jitter,
                    s.rx_core_buffered_access_units,
                    s.rx_ingress_packets,
                    s.rx_ingress_bytes,
                    s.rx_core_missing_sequences_detected,
                    s.rx_core_repaired_packets,
                    s.rx_fec_recovered,
                    s.rx_fec_unrecoverable,
                    s.rx_core_gated,
                    s.rx_receive_target_bps,
                );
            }

            last_tick = Instant::now();
            last_decoded = decoded_now;
            last_native = native_now;
            last_au_received = au_received;
            last_au_fed = au_fed;
            last_au_feed_failures = au_feed_failures;
            last_present_calls = present_calls;
            last_present_true = present_true;
            last_rx_ingress_packets = rx_ingress_packets;
            last_rx_ingress_bytes = rx_ingress_bytes;
            last_rx_missing = rx_missing;
            last_rx_repaired = rx_repaired;
            last_rx_nacks = rx_nacks;
            last_rx_pli = rx_pli;
            last_audio_received = audio_received;
            last_audio_fed = audio_fed;
            last_audio_feed_failures = audio_feed_failures;
        }

        if native_metrics {
            std::thread::sleep(Duration::from_millis(8));
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

/// Scale `src` to fit inside `dst_w`×`dst_h` preserving aspect ratio (letterbox).
fn blit_scaled_fit(src: &[u32], src_w: u32, src_h: u32, dst: &mut [u32], dst_w: u32, dst_h: u32) {
    dst.fill(0);
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    let scale = (dst_w as f32 / src_w as f32).min(dst_h as f32 / src_h as f32);
    let out_w = ((src_w as f32 * scale).floor() as u32).clamp(1, dst_w);
    let out_h = ((src_h as f32 * scale).floor() as u32).clamp(1, dst_h);
    let ox = (dst_w - out_w) / 2;
    let oy = (dst_h - out_h) / 2;
    let inv_scale_x = src_w as f32 / out_w as f32;
    let inv_scale_y = src_h as f32 / out_h as f32;
    for dy in 0..out_h {
        let sy = ((dy as f32 * inv_scale_y) as u32).min(src_h - 1);
        let dst_row = (oy + dy) as usize * dst_w as usize;
        let src_row = sy as usize * src_w as usize;
        for dx in 0..out_w {
            let sx = ((dx as f32 * inv_scale_x) as u32).min(src_w - 1);
            dst[dst_row + (ox + dx) as usize] = src[src_row + sx as usize];
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_scaled_fit_letterboxes_wider_source() {
        let src = vec![0x00FF00u32; 8 * 2]; // 8x2 green (wider than tall)
        let mut dst = vec![0u32; 8 * 8]; // 8x8
        blit_scaled_fit(&src, 8, 2, &mut dst, 8, 8);
        // Letterboxed top/bottom: center row should be green, corners black.
        assert_eq!(dst[8 * 3 + 4], 0x00FF00);
        assert_eq!(dst[0], 0);
        assert_eq!(dst[8 * 7], 0);
    }

    #[test]
    fn default_mode_uses_cpu_window() {
        let args = vec!["probe".to_string()];
        assert_eq!(ProbeMode::from_args(&args), ProbeMode::CpuWindow);
    }

    #[test]
    fn hold_sec_flag_parses() {
        let args = vec![
            "probe".to_string(),
            "--native-metrics".to_string(),
            "--hold-sec".to_string(),
            "65".to_string(),
        ];
        assert_eq!(parse_arg::<u64>(&args, "--hold-sec"), Some(65));
    }
}
