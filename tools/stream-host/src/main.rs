use std::ffi::c_void;
use std::io::Write;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mello_core::stream::host::{self, StartStreamResponse};
use mello_core::stream::StreamConfig;
use mello_core::transport::{SfuConnection, SfuEvent};

const DEFAULT_PORT: u16 = 9800;
const HEADER_CONFIG: u8 = 0x00;
const HEADER_VIDEO: u8 = 0x01;
const HEADER_KEYFRAME: u8 = 0x02;

const CHUNK_HEADER_SIZE: usize = 7; // type(1) + frame_id(2) + chunk_idx(2) + chunk_count(2)
const MAX_CHUNK_PAYLOAD: usize = 60_000;

struct HostState {
    socket: UdpSocket,
    dest: std::net::SocketAddr,
    frame_id: AtomicU16,
    packets_sent: AtomicU32,
    bytes_sent: AtomicU64,
    keyframes_sent: AtomicU32,
    send_errors: AtomicU32,
}

struct NakamaHostContext {
    http_base: String,
    auth_token: String,
    crew_id: String,
}

static RUNNING: AtomicBool = AtomicBool::new(true);
static mut HOST_STATE: *const HostState = std::ptr::null();

unsafe extern "C" fn on_video_packet(
    _user_data: *mut c_void,
    data: *const u8,
    size: i32,
    is_keyframe: bool,
    _ts: u64,
) {
    let state = &*HOST_STATE;
    let payload = std::slice::from_raw_parts(data, size as usize);
    let header = if is_keyframe {
        HEADER_KEYFRAME
    } else {
        HEADER_VIDEO
    };
    let frame_id = state.frame_id.fetch_add(1, Ordering::Relaxed);

    let chunk_count = payload.len().div_ceil(MAX_CHUNK_PAYLOAD);
    let chunk_count = chunk_count.max(1) as u16;

    for i in 0..chunk_count {
        let start = i as usize * MAX_CHUNK_PAYLOAD;
        let end = ((i as usize + 1) * MAX_CHUNK_PAYLOAD).min(payload.len());
        let chunk_data = &payload[start..end];

        let mut buf = Vec::with_capacity(CHUNK_HEADER_SIZE + chunk_data.len());
        buf.push(header);
        buf.extend_from_slice(&frame_id.to_le_bytes());
        buf.extend_from_slice(&i.to_le_bytes());
        buf.extend_from_slice(&chunk_count.to_le_bytes());
        buf.extend_from_slice(chunk_data);

        match state.socket.send_to(&buf, state.dest) {
            Ok(_) => {
                state
                    .bytes_sent
                    .fetch_add(buf.len() as u64, Ordering::Relaxed);
            }
            Err(_) => {
                state.send_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    state.packets_sent.fetch_add(1, Ordering::Relaxed);
    if is_keyframe {
        state.keyframes_sent.fetch_add(1, Ordering::Relaxed);
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    RUNNING.store(true, Ordering::Relaxed);

    let ctx = unsafe { mello_sys::mello_init() };
    if ctx.is_null() {
        eprintln!("ERROR: mello_init() failed");
        std::process::exit(1);
    }

    if !unsafe { mello_sys::mello_encoder_available(ctx) } {
        eprintln!("ERROR: No hardware encoder found (NVENC/AMF/QSV required)");
        unsafe { mello_sys::mello_destroy(ctx) };
        std::process::exit(1);
    }

    // Parse CLI args
    let args: Vec<String> = std::env::args().collect();
    let port = parse_arg(&args, "--port").unwrap_or(DEFAULT_PORT);
    let fps: u32 = parse_arg(&args, "--fps").unwrap_or(60);
    let bitrate: u32 = parse_arg(&args, "--bitrate").unwrap_or(8000);
    let mut sfu_endpoint = parse_arg_string(&args, "--sfu-endpoint");
    let mut sfu_token = parse_arg_string(&args, "--sfu-token");
    let mut sfu_session = parse_arg_string(&args, "--sfu-session");
    let sfu_role = parse_arg_string(&args, "--sfu-role").unwrap_or_else(|| "host".to_string());
    let nakama_start_stream = has_flag(&args, "--nakama-start-stream");
    let mut nakama_host_context: Option<NakamaHostContext> = None;

    if nakama_start_stream {
        if sfu_endpoint.is_some() || sfu_token.is_some() || sfu_session.is_some() {
            eprintln!(
                "ERROR: --nakama-start-stream cannot be combined with manual --sfu-endpoint/--sfu-token/--sfu-session"
            );
            unsafe { mello_sys::mello_destroy(ctx) };
            std::process::exit(1);
        }
        let nakama_http_base = parse_arg_string(&args, "--nakama-http-base").unwrap_or_else(|| {
            eprintln!("Missing --nakama-http-base");
            unsafe { mello_sys::mello_destroy(ctx) };
            std::process::exit(1);
        });
        let nakama_auth_token =
            parse_arg_string(&args, "--nakama-auth-token").unwrap_or_else(|| {
                eprintln!("Missing --nakama-auth-token");
                unsafe { mello_sys::mello_destroy(ctx) };
                std::process::exit(1);
            });
        let crew_id = parse_arg_string(&args, "--crew-id").unwrap_or_else(|| {
            eprintln!("Missing --crew-id");
            unsafe { mello_sys::mello_destroy(ctx) };
            std::process::exit(1);
        });
        let title =
            parse_arg_string(&args, "--stream-title").unwrap_or_else(|| "Stream Host Probe".into());
        let supports_av1 = has_flag(&args, "--supports-av1");
        let req_width = parse_arg(&args, "--request-width").unwrap_or(1920u32);
        let req_height = parse_arg(&args, "--request-height").unwrap_or(1080u32);

        println!("Requesting stream session from Nakama...");
        let start_resp = request_start_stream_via_nakama(
            &nakama_http_base,
            &nakama_auth_token,
            &crew_id,
            &title,
            supports_av1,
            req_width,
            req_height,
        )
        .unwrap_or_else(|e| {
            eprintln!("start_stream RPC failed: {}", e);
            unsafe { mello_sys::mello_destroy(ctx) };
            std::process::exit(1);
        });

        println!(
            "Nakama start_stream: mode={} session={}",
            start_resp.mode,
            start_resp.session_id()
        );
        if start_resp.mode != "sfu" {
            eprintln!(
                "ERROR: start_stream returned mode='{}' (expected 'sfu'). Ensure this crew is SFU-enabled.",
                start_resp.mode
            );
            unsafe { mello_sys::mello_destroy(ctx) };
            std::process::exit(1);
        }

        sfu_endpoint = start_resp.sfu_endpoint.clone();
        sfu_token = start_resp.sfu_token.clone();
        sfu_session = Some(start_resp.session_id());
        nakama_host_context = Some(NakamaHostContext {
            http_base: nakama_http_base,
            auth_token: nakama_auth_token,
            crew_id,
        });
    }

    let (label, source) = select_source(ctx).unwrap_or_else(|| {
        unsafe { mello_sys::mello_destroy(ctx) };
        std::process::exit(1);
    });
    println!("\nStreaming: {}", label);

    // Ctrl+C handler
    ctrlc::set_handler(|| {
        RUNNING.store(false, Ordering::Relaxed);
    })
    .expect("Failed to set Ctrl+C handler");

    let use_sfu = sfu_endpoint.is_some() || sfu_token.is_some() || sfu_session.is_some();
    let result = if use_sfu {
        let endpoint = sfu_endpoint.unwrap_or_else(|| {
            eprintln!("Missing --sfu-endpoint");
            std::process::exit(1);
        });
        let token = sfu_token.unwrap_or_else(|| {
            eprintln!("Missing --sfu-token");
            std::process::exit(1);
        });
        let session = sfu_session.unwrap_or_else(|| {
            eprintln!("Missing --sfu-session");
            std::process::exit(1);
        });
        run_sfu_mode(
            ctx,
            &source,
            fps,
            bitrate,
            &endpoint,
            &token,
            &session,
            &sfu_role,
            nakama_host_context.as_ref(),
        )
    } else {
        run_udp_mode(ctx, &source, fps, bitrate, port)
    };

    if let Err(e) = result {
        eprintln!("ERROR: {}", e);
    }

    unsafe {
        mello_sys::mello_destroy(ctx);
    }
}

fn select_source(
    ctx: *mut mello_sys::MelloContext,
) -> Option<(String, mello_sys::MelloCaptureSource)> {
    println!("\n=== Mello Stream Host ===\n");
    println!("Available capture sources:\n");

    let mut sources: Vec<(String, mello_sys::MelloCaptureSource)> = Vec::new();

    println!("  -- Monitors --");
    for i in 0..2u32 {
        let idx = sources.len();
        sources.push((
            format!("Monitor {}", i),
            mello_sys::MelloCaptureSource {
                mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
                monitor_index: i,
                hwnd: std::ptr::null_mut(),
                pid: 0,
            },
        ));
        println!("  [{}] Monitor {}", idx + 1, i);
    }

    let mut games = vec![
        mello_sys::MelloGameProcess {
            pid: 0,
            name: [0i8; 128],
            exe: [0i8; 260],
            is_fullscreen: false,
        };
        16
    ];
    let game_count = unsafe { mello_sys::mello_enumerate_games(ctx, games.as_mut_ptr(), 16) };
    if game_count > 0 {
        println!("\n  -- Games --");
        for game in games.iter().take(game_count as usize) {
            let name = unsafe { std::ffi::CStr::from_ptr(game.name.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let pid = game.pid;
            let fs = if game.is_fullscreen {
                " [fullscreen]"
            } else {
                ""
            };
            let idx = sources.len();
            sources.push((
                format!("{} (pid {}){}", name, pid, fs),
                mello_sys::MelloCaptureSource {
                    mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                    monitor_index: 0,
                    hwnd: std::ptr::null_mut(),
                    pid,
                },
            ));
            println!("  [{}] {}{} (pid {})", idx + 1, name, fs, pid);
        }
    }

    let mut windows = vec![
        mello_sys::MelloWindow {
            hwnd: std::ptr::null_mut(),
            title: [0i8; 256],
            exe: [0i8; 256],
            pid: 0,
        };
        64
    ];
    let win_count = unsafe { mello_sys::mello_enumerate_windows(ctx, windows.as_mut_ptr(), 64) };
    if win_count > 0 {
        println!("\n  -- Windows --");
        for win in windows.iter().take(win_count as usize) {
            let title = unsafe { std::ffi::CStr::from_ptr(win.title.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let _hwnd = win.hwnd;
            let pid = win.pid;
            let idx = sources.len();
            sources.push((
                format!("{} (pid {})", title, pid),
                mello_sys::MelloCaptureSource {
                    mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                    monitor_index: 0,
                    hwnd: std::ptr::null_mut(),
                    pid,
                },
            ));
            println!("  [{}] {} (pid {})", idx + 1, title, pid);
        }
    }

    print!("\nSelect source: ");
    std::io::stdout().flush().ok()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok()?;
    let choice: usize = match input.trim().parse::<usize>() {
        Ok(n) if n >= 1 && n <= sources.len() => n - 1,
        _ => return None,
    };
    Some(sources[choice].clone())
}

fn run_udp_mode(
    ctx: *mut mello_sys::MelloContext,
    source: &mello_sys::MelloCaptureSource,
    fps: u32,
    bitrate: u32,
    port: u16,
) -> Result<(), String> {
    println!("Config: {}fps {}kbps -> 127.0.0.1:{}\n", fps, bitrate, port);

    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    let dest: std::net::SocketAddr = format!("127.0.0.1:{}", port)
        .parse::<std::net::SocketAddr>()
        .map_err(|e| e.to_string())?;

    let host_state = Arc::new(HostState {
        socket,
        dest,
        frame_id: AtomicU16::new(0),
        packets_sent: AtomicU32::new(0),
        bytes_sent: AtomicU64::new(0),
        keyframes_sent: AtomicU32::new(0),
        send_errors: AtomicU32::new(0),
    });
    unsafe {
        HOST_STATE = Arc::as_ptr(&host_state);
    }

    let config = mello_sys::MelloStreamConfig {
        width: 0,
        height: 0,
        fps,
        bitrate_kbps: bitrate,
    };

    let host = unsafe {
        mello_sys::mello_stream_start_host(
            ctx,
            source,
            &config,
            Some(on_video_packet),
            std::ptr::null_mut(),
        )
    };

    if host.is_null() {
        return Err("mello_stream_start_host() failed".to_string());
    }

    let mut cap_w: u32 = 0;
    let mut cap_h: u32 = 0;
    unsafe { mello_sys::mello_stream_get_host_resolution(host, &mut cap_w, &mut cap_h) };
    println!("Capture resolution: {}x{}", cap_w, cap_h);

    let mut config_pkt = vec![HEADER_CONFIG];
    config_pkt.extend_from_slice(&(cap_w as u16).to_le_bytes());
    config_pkt.extend_from_slice(&(cap_h as u16).to_le_bytes());
    config_pkt.push(fps as u8);
    for _ in 0..3 {
        let _ = host_state.socket.send_to(&config_pkt, dest);
        std::thread::sleep(Duration::from_millis(50));
    }

    println!("Streaming over UDP... (Ctrl+C to stop)\n");

    let start = Instant::now();
    let mut last_packets = 0u32;
    let mut last_bytes = 0u64;
    let mut last_time = Instant::now();
    let mut last_errors = 0u32;

    while RUNNING.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));

        let now = Instant::now();
        let dt = now.duration_since(last_time).as_secs_f64();
        let total_packets = host_state.packets_sent.load(Ordering::Relaxed);
        let total_bytes = host_state.bytes_sent.load(Ordering::Relaxed);
        let total_keyframes = host_state.keyframes_sent.load(Ordering::Relaxed);
        let total_errors = host_state.send_errors.load(Ordering::Relaxed);

        let pps = (total_packets - last_packets) as f64 / dt.max(0.001);
        let bps = ((total_bytes - last_bytes) as f64 * 8.0) / dt.max(0.001) / 1000.0;
        let elapsed = start.elapsed().as_secs();

        let err_str = if total_errors > last_errors {
            format!(" send_err={}", total_errors - last_errors)
        } else {
            String::new()
        };

        let _ = host_state.socket.send_to(&config_pkt, dest);

        print!(
            "\r[{:3}s] fps={:.0} bitrate={:.0}kbps keyframes={} total={:.1}MB{}   ",
            elapsed,
            pps,
            bps,
            total_keyframes,
            total_bytes as f64 / (1024.0 * 1024.0),
            err_str,
        );
        std::io::stdout().flush().map_err(|e| e.to_string())?;

        last_packets = total_packets;
        last_bytes = total_bytes;
        last_time = now;
        last_errors = total_errors;
    }

    println!("\n\nStopping...");
    unsafe {
        mello_sys::mello_stream_stop_host(host);
        HOST_STATE = std::ptr::null();
    }
    println!("Done.");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_sfu_mode(
    ctx: *mut mello_sys::MelloContext,
    source: &mello_sys::MelloCaptureSource,
    fps: u32,
    bitrate: u32,
    endpoint: &str,
    token: &str,
    session_id: &str,
    role: &str,
    nakama_host_context: Option<&NakamaHostContext>,
) -> Result<(), String> {
    let correlation_start = Instant::now();
    let correlation_epoch_ms = unix_time_ms();
    println!("Config: {}fps {}kbps -> SFU {}", fps, bitrate, endpoint);
    println!("Session: {} (role={})\n", session_id, role);
    println!("corr_start_unix_ms: {}", correlation_epoch_ms);
    log::info!(
        "host_probe_start session={} wall_ms={} mono_ms=0 role={} endpoint={}",
        session_id,
        correlation_epoch_ms,
        role,
        endpoint
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    let mut conn = rt
        .block_on(SfuConnection::connect(endpoint, token))
        .map_err(|e| format!("SFU connect failed: {}", e))?;
    let peer_handle = unsafe { SfuConnection::create_peer(ctx) }
        .map_err(|e| format!("SFU peer creation failed: {}", e))?;
    rt.block_on(conn.join_stream(peer_handle, session_id, role))
        .map_err(|e| format!("SFU join_stream failed: {}", e))?;
    rt.block_on(conn.wait_for_datachannel_open())
        .map_err(|e| format!("SFU datachannel open failed: {}", e))?;

    let conn = Arc::new(conn);
    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
    log::info!(
        "host_probe_event session={} wall_ms={} mono_ms={} event=host_ready server={} region={} rtt_ms={:.1}",
        session_id,
        wall_ms,
        mono_ms,
        conn.server_id(),
        conn.region(),
        conn.rtt_ms()
    );

    let mello_config = mello_sys::MelloStreamConfig {
        width: 0,
        height: 0,
        fps,
        bitrate_kbps: bitrate,
    };
    let (host, video_rx, audio_rx, resources) =
        unsafe { host::start_host(ctx, source, &mello_config) }
            .map_err(|e| format!("start_host failed: {}", e))?;

    unsafe {
        mello_sys::mello_stream_start_audio(host);
    }

    let mut cap_w: u32 = 0;
    let mut cap_h: u32 = 0;
    unsafe {
        mello_sys::mello_stream_get_host_resolution(host, &mut cap_w, &mut cap_h);
    }
    println!("Capture resolution: {}x{}", cap_w, cap_h);
    if let Some(ctx) = nakama_host_context {
        if let Err(e) = request_update_stream_resolution_via_nakama(
            &ctx.http_base,
            &ctx.auth_token,
            &ctx.crew_id,
            cap_w,
            cap_h,
        ) {
            log::warn!("update_stream_resolution RPC failed: {}", e);
        } else {
            let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
            log::info!(
                "host_probe_event session={} wall_ms={} mono_ms={} event=updated_backend_resolution crew={} width={} height={}",
                session_id,
                wall_ms,
                mono_ms,
                ctx.crew_id,
                cap_w,
                cap_h
            );
        }
    }
    println!("Streaming over SFU... (Ctrl+C to stop)\n");

    let sink: Arc<dyn mello_core::stream::sink::PacketSink> = Arc::new(
        mello_core::stream::sink_sfu::SfuSink::new(Arc::clone(&conn)),
    );
    let resp = StartStreamResponse {
        session_id: Some(session_id.to_string()),
        stream_id: None,
        mode: "sfu".to_string(),
        max_viewers: None,
        sfu_endpoint: Some(endpoint.to_string()),
        sfu_token: None,
    };

    let cfg = StreamConfig {
        width: cap_w.max(1),
        height: cap_h.max(1),
        fps,
        bitrate_kbps: bitrate,
        ..StreamConfig::default()
    };

    let mut stream_session = rt
        .block_on(async {
            host::create_stream_session(ctx, host, &resp, cfg, video_rx, audio_rx, resources, sink)
        })
        .map_err(|e| format!("create_stream_session failed: {}", e))?;

    let start = Instant::now();
    let mut joined_viewers: i32 = 0;
    let mut last_disconnect: Option<String> = None;
    let mut channel_closed_streak: u32 = 0;

    while RUNNING.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        conn.send_ping();
        for ev in conn.poll_events() {
            match ev {
                SfuEvent::MemberJoined { user_id, role } => {
                    joined_viewers += 1;
                    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                    log::info!(
                        "host_probe_event session={} wall_ms={} mono_ms={} event=member_joined user={} role={}",
                        session_id,
                        wall_ms,
                        mono_ms,
                        user_id,
                        role
                    );
                    unsafe {
                        mello_sys::mello_stream_request_keyframe(host);
                    }
                    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                    log::info!(
                        "host_probe_event session={} wall_ms={} mono_ms={} event=requested_keyframe reason=viewer_join",
                        session_id,
                        wall_ms,
                        mono_ms
                    );
                }
                SfuEvent::MemberLeft { user_id, reason } => {
                    joined_viewers = (joined_viewers - 1).max(0);
                    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                    log::info!(
                        "host_probe_event session={} wall_ms={} mono_ms={} event=member_left user={} reason={}",
                        session_id,
                        wall_ms,
                        mono_ms,
                        user_id,
                        reason
                    );
                }
                SfuEvent::Disconnected { reason } => {
                    last_disconnect = Some(reason.clone());
                    let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                    log::warn!(
                        "host_probe_event session={} wall_ms={} mono_ms={} event=disconnected reason={}",
                        session_id,
                        wall_ms,
                        mono_ms,
                        reason
                    );
                    RUNNING.store(false, Ordering::Relaxed);
                }
                _ => {}
            }
        }

        let media_open = conn.is_media_channel_open();
        let control_open = conn.is_control_channel_open();
        if media_open || control_open {
            channel_closed_streak = 0;
        } else {
            channel_closed_streak = channel_closed_streak.saturating_add(1);
            if channel_closed_streak == 1 {
                let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                log::warn!(
                    "host_probe_event session={} wall_ms={} mono_ms={} event=channels_closed media_open=false control_open=false",
                    session_id,
                    wall_ms,
                    mono_ms
                );
            }
            if channel_closed_streak >= 3 {
                let reason = "media/control channels stayed closed".to_string();
                last_disconnect = Some(reason.clone());
                let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                log::warn!(
                    "host_probe_event session={} wall_ms={} mono_ms={} event=stopping reason={}",
                    session_id,
                    wall_ms,
                    mono_ms,
                    reason
                );
                RUNNING.store(false, Ordering::Relaxed);
            }
        }

        let elapsed = start.elapsed().as_secs();
        let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
        let disconnect = last_disconnect.clone().unwrap_or_else(|| "-".to_string());
        log::info!(
            "host_probe_tick session={} wall_ms={} mono_ms={} viewers={} media_open={} control_open={} rtt_ms={:.1} disconnect={}",
            session_id,
            wall_ms,
            mono_ms,
            joined_viewers,
            media_open,
            control_open,
            conn.rtt_ms(),
            disconnect
        );
        print!(
            "\r[{:3}s] viewers={} media_open={} control_open={} rtt_ms={:.1} disconnect={}   ",
            elapsed,
            joined_viewers,
            media_open,
            control_open,
            conn.rtt_ms(),
            disconnect,
        );
        std::io::stdout().flush().map_err(|e| e.to_string())?;
    }

    println!("\n\nStopping...");
    stream_session.stop();
    rt.block_on(async {
        conn.leave().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    println!("Done.");
    Ok(())
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

fn request_start_stream_via_nakama(
    http_base: &str,
    auth_token: &str,
    crew_id: &str,
    title: &str,
    supports_av1: bool,
    width: u32,
    height: u32,
) -> Result<StartStreamResponse, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    rt.block_on(async move {
        let url = format!("{}/v2/rpc/start_stream", http_base.trim_end_matches('/'));
        let payload = serde_json::json!({
            "crew_id": crew_id,
            "title": title,
            "supports_av1": supports_av1,
            "width": width,
            "height": height,
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
        serde_json::from_str::<StartStreamResponse>(payload).map_err(|e| e.to_string())
    })
}

fn request_update_stream_resolution_via_nakama(
    http_base: &str,
    auth_token: &str,
    crew_id: &str,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    rt.block_on(async move {
        let url = format!(
            "{}/v2/rpc/update_stream_resolution",
            http_base.trim_end_matches('/')
        );
        let payload = serde_json::json!({
            "crew_id": crew_id,
            "width": width,
            "height": height,
        });
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
        Ok(())
    })
}
