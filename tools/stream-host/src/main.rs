use std::ffi::c_void;
use std::io::Write;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_PORT: u16 = 9800;
const HEADER_CONFIG: u8 = 0x00;
const HEADER_VIDEO: u8 = 0x01;
const HEADER_KEYFRAME: u8 = 0x02;

struct HostState {
    socket: UdpSocket,
    dest: std::net::SocketAddr,
    packets_sent: AtomicU32,
    bytes_sent: AtomicU64,
    keyframes_sent: AtomicU32,
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
    let mut buf = Vec::with_capacity(1 + payload.len());
    buf.push(header);
    buf.extend_from_slice(payload);

    let _ = state.socket.send_to(&buf, state.dest);
    state.packets_sent.fetch_add(1, Ordering::Relaxed);
    state.bytes_sent.fetch_add(buf.len() as u64, Ordering::Relaxed);
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
        packets_sent: AtomicU32::new(0),
        bytes_sent: AtomicU64::new(0),
        keyframes_sent: AtomicU32::new(0),
    });
    unsafe {
        HOST_STATE = Arc::as_ptr(&host_state);
    }

    // Ctrl+C handler
    ctrlc::set_handler(|| {
        RUNNING.store(false, Ordering::Relaxed);
    })
    .expect("Failed to set Ctrl+C handler");

    // Send config handshake: [0x00][width_u16_le][height_u16_le][fps_u8]
    // We'll use 0 for width/height to signal "native resolution" —
    // the viewer needs to get actual resolution from the first frame.
    // For now send a fixed config derived from the flags or defaults.
    let config_w: u16 = 0; // 0 = native (viewer will learn from decoded frame)
    let config_h: u16 = 0;
    let mut config_pkt = vec![HEADER_CONFIG];
    config_pkt.extend_from_slice(&config_w.to_le_bytes());
    config_pkt.extend_from_slice(&config_h.to_le_bytes());
    config_pkt.push(fps as u8);
    let _ = host_state.socket.send_to(&config_pkt, dest);

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

    println!("Streaming... (Ctrl+C to stop)\n");

    // Stats loop
    let start = Instant::now();
    let mut last_packets = 0u32;
    let mut last_bytes = 0u64;
    let mut last_time = Instant::now();

    while RUNNING.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));

        let now = Instant::now();
        let dt = now.duration_since(last_time).as_secs_f64();
        let total_packets = host_state.packets_sent.load(Ordering::Relaxed);
        let total_bytes = host_state.bytes_sent.load(Ordering::Relaxed);
        let total_keyframes = host_state.keyframes_sent.load(Ordering::Relaxed);

        let pps = (total_packets - last_packets) as f64 / dt;
        let bps = ((total_bytes - last_bytes) as f64 * 8.0) / dt / 1000.0;
        let elapsed = start.elapsed().as_secs();

        print!(
            "\r[{:3}s] fps={:.0} bitrate={:.0}kbps keyframes={} total={:.1}MB   ",
            elapsed,
            pps,
            bps,
            total_keyframes,
            total_bytes as f64 / (1024.0 * 1024.0)
        );
        std::io::stdout().flush().unwrap();

        last_packets = total_packets;
        last_bytes = total_bytes;
        last_time = now;
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
