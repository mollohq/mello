use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod bench;

use mello_core::stream::host::{self, StartStreamResponse};
use mello_core::stream::sink::PacketSink;
use mello_core::stream::StreamConfig;
use mello_core::transport::{SfuConnection, SfuEvent, StreamPeerRole};

struct NakamaHostContext {
    http_base: String,
    auth_token: String,
    crew_id: String,
}

static RUNNING: AtomicBool = AtomicBool::new(true);

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
    let fps: u32 = parse_arg(&args, "--fps").unwrap_or(60);
    let bitrate: u32 = parse_arg(&args, "--bitrate").unwrap_or(8000);
    let mut sfu_endpoint = parse_arg_string(&args, "--sfu-endpoint");
    let mut sfu_token = parse_arg_string(&args, "--sfu-token");
    let mut sfu_session = parse_arg_string(&args, "--sfu-session");
    let sfu_role = parse_arg_string(&args, "--sfu-role").unwrap_or_else(|| "host".to_string());
    let source_index = parse_arg::<usize>(&args, "--source-index");
    let source_title_substring = parse_arg_string(&args, "--source-title-substring");
    let nakama_start_stream = has_flag(&args, "--nakama-start-stream");
    let mut nakama_host_context: Option<NakamaHostContext> = None;

    // Local capture benchmark (no network): DXGI vs WGC, plan section 2.6.
    if let Some(csv_path) = parse_arg_string(&args, "--bench-csv") {
        let backend = parse_arg_string(&args, "--capture-backend")
            .and_then(|b| bench::BenchBackend::parse(&b))
            .unwrap_or_else(|| {
                eprintln!(
                    "ERROR: --capture-backend must be dxgi, wgc-monitor, wgc-window or process"
                );
                unsafe { mello_sys::mello_destroy(ctx) };
                std::process::exit(1);
            });
        let monitor_index: u32 = parse_arg(&args, "--monitor-index").unwrap_or(0);
        let seconds: u64 = parse_arg(&args, "--bench-seconds").unwrap_or(60);
        let window = source_title_substring
            .as_deref()
            .and_then(|needle| bench::find_window(ctx, needle));
        let label = match &window {
            Some((_, pid, title)) => format!("{:?} {} (pid {})", backend, title, pid),
            None => format!("{:?} monitor {}", backend, monitor_index),
        };
        // --allow-hook stands in for the game catalogue and the backend
        // switch, which the client has and this tool does not. It only makes
        // the hook step available; libmello still runs every run-time check
        // before it injects.
        let allow_hook = has_flag(&args, "--allow-hook");
        let source = match bench::source_for(
            backend,
            monitor_index,
            window.as_ref().map(|(hwnd, pid, _)| (*hwnd, *pid)),
            allow_hook,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ERROR: {}", e);
                unsafe { mello_sys::mello_destroy(ctx) };
                std::process::exit(1);
            }
        };
        ctrlc::set_handler(|| {
            RUNNING.store(false, Ordering::Relaxed);
        })
        .expect("Failed to set Ctrl+C handler");
        let opts = bench::BenchOptions {
            label,
            fps,
            bitrate_kbps: bitrate,
            seconds,
            csv_path,
        };
        if let Err(e) = bench::run(ctx, &source, &opts, &RUNNING) {
            eprintln!("ERROR: {}", e);
        }
        unsafe { mello_sys::mello_destroy(ctx) };
        return;
    }

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
            bitrate,
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

    let (label, source) = select_source(ctx, source_index, source_title_substring.as_deref())
        .unwrap_or_else(|| {
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
        eprintln!("ERROR: no SFU target. Use --nakama-start-stream or --sfu-*.");
        unsafe { mello_sys::mello_destroy(ctx) };
        std::process::exit(1);
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
    source_index: Option<usize>,
    source_title_substring: Option<&str>,
) -> Option<(String, mello_sys::MelloCaptureSource)> {
    println!("\n=== Mello Stream Host ===\n");
    println!("Available capture sources:\n");

    let sources = enumerate_sources(ctx);
    for (idx, (label, _)) in sources.iter().enumerate() {
        println!("  [{}] {}", idx + 1, label);
    }

    let choice = if let Some(index) = source_index {
        if index >= 1 && index <= sources.len() {
            index - 1
        } else {
            eprintln!(
                "ERROR: --source-index {} out of range (1..={})",
                index,
                sources.len()
            );
            return None;
        }
    } else if let Some(needle) = source_title_substring {
        let needle = needle.to_ascii_lowercase();
        match sources
            .iter()
            .position(|(label, _)| label.to_ascii_lowercase().contains(&needle))
        {
            Some(idx) => idx,
            None => {
                eprintln!(
                    "ERROR: no capture source matches --source-title-substring '{}'",
                    source_title_substring.unwrap_or("")
                );
                return None;
            }
        }
    } else {
        print!("\nSelect source: ");
        std::io::stdout().flush().ok()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok()?;
        match input.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= sources.len() => n - 1,
            _ => return None,
        }
    };

    Some(sources[choice].clone())
}

fn enumerate_sources(
    ctx: *mut mello_sys::MelloContext,
) -> Vec<(String, mello_sys::MelloCaptureSource)> {
    let mut sources: Vec<(String, mello_sys::MelloCaptureSource)> = Vec::new();

    for i in 0..2u32 {
        sources.push((
            format!("Monitor {}", i),
            mello_sys::MelloCaptureSource {
                mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
                monitor_index: i,
                hwnd: std::ptr::null_mut(),
                pid: 0,
                allow_hook: false,
                pipewire_fd: -1,
                pipewire_node: 0,
            },
        ));
    }

    let mut games = vec![
        mello_sys::MelloGameProcess {
            pid: 0,
            name: [0i8; 128],
            exe: [0i8; 260],
            is_fullscreen: false,
            path: [0i8; 520],
            title: [0i8; 256],
            is_foreground: false,
            started_at_ms: 0,
        };
        16
    ];
    let game_count = unsafe { mello_sys::mello_enumerate_games(ctx, games.as_mut_ptr(), 16) };
    if game_count > 0 {
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
            sources.push((
                format!("{} (pid {}){}", name, pid, fs),
                mello_sys::MelloCaptureSource {
                    mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                    monitor_index: 0,
                    hwnd: std::ptr::null_mut(),
                    pid,
                    allow_hook: false,
                    pipewire_fd: -1,
                    pipewire_node: 0,
                },
            ));
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
        for win in windows.iter().take(win_count as usize) {
            let title = unsafe { std::ffi::CStr::from_ptr(win.title.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let pid = win.pid;
            sources.push((
                format!("{} (pid {})", title, pid),
                mello_sys::MelloCaptureSource {
                    mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                    monitor_index: 0,
                    hwnd: std::ptr::null_mut(),
                    pid,
                    allow_hook: false,
                    pipewire_fd: -1,
                    pipewire_node: 0,
                },
            ));
        }
    }

    sources
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
    let peer_handle = unsafe { SfuConnection::create_stream_peer(ctx, StreamPeerRole::Host, &[]) }
        .map_err(|e| format!("SFU stream peer creation failed: {}", e))?;
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

    let sink: Arc<mello_core::stream::sink_sfu::SfuSink> = Arc::new(
        mello_core::stream::sink_sfu::SfuSink::new(Arc::clone(&conn)),
    );
    let sink_for_events: Arc<dyn mello_core::stream::sink::PacketSink> = Arc::clone(&sink) as _;
    let resp = StartStreamResponse {
        session_id: Some(session_id.to_string()),
        stream_id: None,
        mode: "sfu".to_string(),
        sfu_endpoint: Some(endpoint.to_string()),
        sfu_token: None,
        capture: None,
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
            host::create_stream_session(
                ctx,
                host,
                &resp,
                cfg,
                video_rx,
                audio_rx,
                resources,
                sink_for_events,
                None,
            )
        })
        .map_err(|e| format!("create_stream_session failed: {}", e))?;

    let start = Instant::now();
    let mut joined_viewers: i32 = 0;
    let mut last_disconnect: Option<String> = None;
    let mut channel_closed_streak: u32 = 0;

    let mut last_stats = conn
        .video_stats()
        .unwrap_or_else(|_| unsafe { std::mem::zeroed() });

    while RUNNING.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        conn.send_ping();
        for ev in conn.poll_events() {
            match ev {
                SfuEvent::MemberJoined { user_id, role } => {
                    if role == "viewer" {
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
                        rt.block_on(async {
                            sink.on_viewer_joined(&user_id).await;
                        });
                    }
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
                    rt.block_on(async {
                        sink.on_viewer_left(&user_id).await;
                    });
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

        let video_open = conn.is_video_track_open();
        let control_open = conn.is_control_channel_open();
        if video_open && control_open {
            channel_closed_streak = 0;
        } else {
            channel_closed_streak = channel_closed_streak.saturating_add(1);
            if channel_closed_streak == 1 {
                let (wall_ms, mono_ms) = correlation_stamp(correlation_start);
                log::warn!(
                    "host_probe_event session={} wall_ms={} mono_ms={} event=transport_closed video_open={} control_open={}",
                    session_id,
                    wall_ms,
                    mono_ms,
                    video_open,
                    control_open
                );
            }
            if channel_closed_streak >= 3 {
                let reason = "control/video transport stayed closed".to_string();
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
        let stats_now = conn
            .video_stats()
            .unwrap_or_else(|_| unsafe { std::mem::zeroed() });
        log::info!(
            "host_probe_tick session={} wall_ms={} mono_ms={} viewers={} video_open={} control_open={} rtt_ms={:.1} disconnect={} tx_aus_sent={} tx_aus_dropped={} tx_aus_rejected={} tx_rtp_packets={} tx_rtp_bytes={} tx_queue_peak={} tx_pacing_target_kbps={} tx_gcc_target_kbps={} tx_pacing_delay_us={} tx_fec_packets={} tx_rtx_sent={} tx_pli={} tx_remb={}",
            session_id,
            wall_ms,
            mono_ms,
            joined_viewers,
            video_open,
            control_open,
            conn.rtt_ms(),
            disconnect,
            stats_now.tx_access_units_sent,
            stats_now.tx_access_units_dropped,
            stats_now.tx_access_units_rejected,
            stats_now.tx_rtp_packets_sent,
            stats_now.tx_rtp_wire_bytes_sent,
            stats_now.tx_peak_queued_access_units,
            stats_now.tx_pacing_target_bps / 1_000,
            stats_now.tx_gcc_target_bps / 1_000,
            stats_now.tx_current_pacing_delay_us,
            stats_now.tx_fec_packets_sent,
            stats_now.tx_rtx_sent,
            stats_now.tx_pli_requests,
            stats_now.tx_remb_reports,
        );
        print!(
            "\r[{:3}s] viewers={} video_open={} control_open={} tx_aus={} tx_drop={} tx_fec={} tx_gcc_kbps={} rtt_ms={:.1} disconnect={}   ",
            elapsed,
            joined_viewers,
            video_open,
            control_open,
            stats_now.tx_access_units_sent.saturating_sub(last_stats.tx_access_units_sent),
            stats_now.tx_access_units_dropped.saturating_sub(last_stats.tx_access_units_dropped),
            stats_now.tx_fec_packets_sent.saturating_sub(last_stats.tx_fec_packets_sent),
            stats_now.tx_gcc_target_bps / 1_000,
            conn.rtt_ms(),
            disconnect,
        );
        last_stats = stats_now;
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

#[allow(clippy::too_many_arguments)]
fn request_start_stream_via_nakama(
    http_base: &str,
    auth_token: &str,
    crew_id: &str,
    title: &str,
    supports_av1: bool,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
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
            "bitrate_kbps": bitrate_kbps,
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
