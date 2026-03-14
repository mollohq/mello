use std::ffi::c_void;
use std::io::Write;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    let header = if is_keyframe { HEADER_KEYFRAME } else { HEADER_VIDEO };
    let frame_id = state.frame_id.fetch_add(1, Ordering::Relaxed);

    let chunk_count = (payload.len() + MAX_CHUNK_PAYLOAD - 1) / MAX_CHUNK_PAYLOAD;
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
                state.bytes_sent.fetch_add(buf.len() as u64, Ordering::Relaxed);
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

    // Enumerate sources
    println!("\n=== Mello Stream Host ===\n");
    println!("Available capture sources:\n");

    let mut sources: Vec<(String, mello_sys::MelloCaptureSource)> = Vec::new();

    // Monitors
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

    // Game processes
    let mut games = vec![mello_sys::MelloGameProcess {
        pid: 0,
        name: [0i8; 128],
        exe: [0i8; 260],
        is_fullscreen: false,
    }; 16];
    let game_count = unsafe { mello_sys::mello_enumerate_games(ctx, games.as_mut_ptr(), 16) };
    if game_count > 0 {
        println!("\n  -- Games --");
        for i in 0..game_count as usize {
            let name = unsafe { std::ffi::CStr::from_ptr(games[i].name.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let pid = games[i].pid;
            let fs = if games[i].is_fullscreen {
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

    // Visible windows
    let mut windows = vec![
        mello_sys::MelloWindow {
            hwnd: std::ptr::null_mut(),
            title: [0i8; 256],
            pid: 0,
        };
        64
    ];
    let win_count = unsafe { mello_sys::mello_enumerate_windows(ctx, windows.as_mut_ptr(), 64) };
    if win_count > 0 {
        println!("\n  -- Windows --");
        for i in 0..win_count as usize {
            let title = unsafe { std::ffi::CStr::from_ptr(windows[i].title.as_ptr()) }
                .to_string_lossy()
                .to_string();
            let hwnd = windows[i].hwnd;
            let pid = windows[i].pid;
            let idx = sources.len();
            sources.push((
                format!("{} (pid {})", title, pid),
                mello_sys::MelloCaptureSource {
                    mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_WINDOW,
                    monitor_index: 0,
                    hwnd,
                    pid: 0,
                },
            ));
            println!("  [{}] {} (pid {})", idx + 1, title, pid);
        }
    }

    print!("\nSelect source: ");
    std::io::stdout().flush().unwrap();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
    let choice: usize = match input.trim().parse::<usize>() {
        Ok(n) if n >= 1 && n <= sources.len() => n - 1,
        _ => {
            eprintln!("Invalid selection");
            unsafe { mello_sys::mello_destroy(ctx) };
            std::process::exit(1);
        }
    };

    let (label, source) = &sources[choice];
    println!("\nStreaming: {}", label);
    println!("Config: {}fps {}kbps -> 127.0.0.1:{}\n", fps, bitrate, port);

    // Set up UDP
    let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind UDP socket");
    let dest: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

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

    // Ctrl+C handler
    ctrlc::set_handler(|| {
        RUNNING.store(false, Ordering::Relaxed);
    })
    .expect("Failed to set Ctrl+C handler");

    // Start host pipeline (captures at native resolution of the source)
    let config = mello_sys::MelloStreamConfig {
        width: 0,  // 0 = use capture source native resolution
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
        eprintln!("ERROR: mello_stream_start_host() failed");
        unsafe { mello_sys::mello_destroy(ctx) };
        std::process::exit(1);
    }

    // Query actual capture resolution and send config handshake to viewer
    let mut cap_w: u32 = 0;
    let mut cap_h: u32 = 0;
    unsafe { mello_sys::mello_stream_get_host_resolution(host, &mut cap_w, &mut cap_h) };
    println!("Capture resolution: {}x{}", cap_w, cap_h);

    // Config packet uses the old format (no chunking needed, it's tiny)
    let config_pkt_w = cap_w as u16;
    let config_pkt_h = cap_h as u16;
    let mut config_pkt = vec![HEADER_CONFIG];
    config_pkt.extend_from_slice(&config_pkt_w.to_le_bytes());
    config_pkt.extend_from_slice(&config_pkt_h.to_le_bytes());
    config_pkt.push(fps as u8);
    if let Err(e) = host_state.socket.send_to(&config_pkt, dest) {
        log::warn!("Failed to send config packet: {}", e);
    }

    println!("Streaming... (Ctrl+C to stop)\n");

    // Stats loop
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

        let pps = (total_packets - last_packets) as f64 / dt;
        let bps = ((total_bytes - last_bytes) as f64 * 8.0) / dt / 1000.0;
        let elapsed = start.elapsed().as_secs();

        let err_str = if total_errors > last_errors {
            format!(" send_err={}", total_errors - last_errors)
        } else {
            String::new()
        };

        print!(
            "\r[{:3}s] fps={:.0} bitrate={:.0}kbps keyframes={} total={:.1}MB{}   ",
            elapsed, pps, bps, total_keyframes,
            total_bytes as f64 / (1024.0 * 1024.0),
            err_str,
        );
        std::io::stdout().flush().unwrap();

        last_packets = total_packets;
        last_bytes = total_bytes;
        last_time = now;
        last_errors = total_errors;
    }

    println!("\n\nStopping...");
    unsafe {
        mello_sys::mello_stream_stop_host(host);
        HOST_STATE = std::ptr::null();
        mello_sys::mello_destroy(ctx);
    }
    println!("Done.");
}

fn parse_arg<T: std::str::FromStr>(args: &[String], flag: &str) -> Option<T> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}
