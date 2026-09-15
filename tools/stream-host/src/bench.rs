//! Capture benchmark: DXGI desktop duplication against Windows Graphics
//! Capture (streaming reliability plan, section 2.6).
//!
//! Runs the real host pipeline (capture, GPU convert, hardware encode) with no
//! network, samples `mello_stream_get_stats` once per second, and writes one
//! CSV row per second. Game frame-rate cost is measured separately with
//! PresentMon; glass-to-glass with a camera.
//!
//! ```text
//! stream-host --bench-csv out.csv --capture-backend dxgi --monitor-index 0 --bench-seconds 60
//! stream-host --bench-csv out.csv --capture-backend wgc-monitor --monitor-index 0
//! stream-host --bench-csv out.csv --capture-backend wgc-window --source-title-substring "Cyberpunk"
//! stream-host --bench-csv out.csv --capture-backend process --source-title-substring "Cyberpunk"
//! ```

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const HIST_BUCKETS: usize = 32;

/// Capture method under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchBackend {
    Dxgi,
    WgcMonitor,
    WgcWindow,
    Process,
}

impl BenchBackend {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dxgi" => Some(Self::Dxgi),
            "wgc-monitor" => Some(Self::WgcMonitor),
            "wgc-window" => Some(Self::WgcWindow),
            "process" => Some(Self::Process),
            _ => None,
        }
    }
}

/// Percentile (0..=100) of a 1 ms bucket histogram, as the bucket's upper edge
/// in ms. None when the histogram is empty.
pub fn histogram_percentile_ms(hist: &[u64; HIST_BUCKETS], percentile: f64) -> Option<u32> {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return None;
    }
    let target = ((percentile / 100.0) * total as f64).ceil().max(1.0) as u64;
    let mut seen = 0u64;
    for (i, count) in hist.iter().enumerate() {
        seen += count;
        if seen >= target {
            return Some(i as u32 + 1);
        }
    }
    Some(HIST_BUCKETS as u32)
}

fn diff_hist(now: &[u32; HIST_BUCKETS], before: &[u32; HIST_BUCKETS]) -> [u64; HIST_BUCKETS] {
    let mut out = [0u64; HIST_BUCKETS];
    for i in 0..HIST_BUCKETS {
        out[i] = now[i].wrapping_sub(before[i]) as u64;
    }
    out
}

fn cstr(raw: &[std::os::raw::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

static ENCODED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn on_bench_packet(
    _user_data: *mut std::ffi::c_void,
    _data: *const u8,
    size: i32,
    _is_keyframe: bool,
    _timestamp_us: u64,
) {
    if size > 0 {
        ENCODED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    }
}

/// Build the capture source for a backend. `window` is (hwnd, pid) of the
/// matched window, needed for `wgc-window` and `process`.
pub fn source_for(
    backend: BenchBackend,
    monitor_index: u32,
    window: Option<(*mut std::ffi::c_void, u32)>,
) -> Result<mello_sys::MelloCaptureSource, String> {
    let blank = mello_sys::MelloCaptureSource {
        mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR,
        monitor_index: 0,
        hwnd: std::ptr::null_mut(),
        pid: 0,
    };
    Ok(match backend {
        BenchBackend::Dxgi => mello_sys::MelloCaptureSource {
            monitor_index,
            ..blank
        },
        BenchBackend::WgcMonitor => mello_sys::MelloCaptureSource {
            mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_MONITOR_WGC,
            monitor_index,
            ..blank
        },
        BenchBackend::WgcWindow => {
            let (hwnd, _) = window.ok_or("wgc-window needs --source-title-substring")?;
            mello_sys::MelloCaptureSource {
                mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_WINDOW,
                hwnd,
                ..blank
            }
        }
        BenchBackend::Process => {
            let (_, pid) = window.ok_or("process needs --source-title-substring")?;
            mello_sys::MelloCaptureSource {
                mode: mello_sys::MelloCaptureMode_MELLO_CAPTURE_PROCESS,
                pid,
                ..blank
            }
        }
    })
}

/// Find a top-level window whose title contains `needle` (case-insensitive).
pub fn find_window(
    ctx: *mut mello_sys::MelloContext,
    needle: &str,
) -> Option<(*mut std::ffi::c_void, u32, String)> {
    let mut windows = vec![
        mello_sys::MelloWindow {
            hwnd: std::ptr::null_mut(),
            title: [0; 256],
            exe: [0; 256],
            pid: 0,
        };
        128
    ];
    let count = unsafe { mello_sys::mello_enumerate_windows(ctx, windows.as_mut_ptr(), 128) };
    let needle = needle.to_ascii_lowercase();
    windows
        .iter()
        .take(count.max(0) as usize)
        .map(|w| (w.hwnd, w.pid, cstr(&w.title)))
        .find(|(_, _, title)| title.to_ascii_lowercase().contains(&needle))
}

/// Benchmark run settings.
pub struct BenchOptions {
    pub label: String,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub seconds: u64,
    pub csv_path: String,
}

/// Run the benchmark until `opts.seconds` pass or `running` turns false.
pub fn run(
    ctx: *mut mello_sys::MelloContext,
    source: &mello_sys::MelloCaptureSource,
    opts: &BenchOptions,
    running: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let (label, fps, bitrate_kbps, seconds, csv_path) = (
        opts.label.as_str(),
        opts.fps,
        opts.bitrate_kbps,
        opts.seconds,
        opts.csv_path.as_str(),
    );
    let config = mello_sys::MelloStreamConfig {
        width: 1280,
        height: 720,
        fps,
        bitrate_kbps,
    };
    let host = unsafe {
        mello_sys::mello_stream_start_host(
            ctx,
            source,
            &config,
            Some(on_bench_packet),
            std::ptr::null_mut(),
        )
    };
    if host.is_null() {
        return Err("mello_stream_start_host failed".into());
    }

    let mut file = std::fs::File::create(csv_path).map_err(|e| e.to_string())?;
    writeln!(
        file,
        "t_s,label,backend,capture_fps,encode_fps,capture_idle_ms,convert_ms,encode_ms_mean,\
eq_drops,idle_repeats,kbps,delay_frames,delay_p50_ms,delay_p95_ms,delay_p99_ms,capture_failed,history"
    )
    .map_err(|e| e.to_string())?;

    let mut prev: mello_sys::MelloStreamStats = unsafe { std::mem::zeroed() };
    unsafe { mello_sys::mello_stream_get_stats(host, &mut prev) };
    let first_hist = prev.present_delay_hist;
    let mut prev_bytes = ENCODED_BYTES.load(Ordering::Relaxed);
    let start = Instant::now();
    let mut last = Instant::now();

    println!("Benchmark: {label} for {seconds} s -> {csv_path}");
    while running.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_secs(1));
        let mut s: mello_sys::MelloStreamStats = unsafe { std::mem::zeroed() };
        unsafe { mello_sys::mello_stream_get_stats(host, &mut s) };
        let dt = last.elapsed().as_secs_f64().max(0.001);
        last = Instant::now();

        let hist = diff_hist(&s.present_delay_hist, &prev.present_delay_hist);
        let frames: u64 = hist.iter().sum();
        let bytes = ENCODED_BYTES.load(Ordering::Relaxed);
        let fmt_p = |p: f64| {
            histogram_percentile_ms(&hist, p)
                .map(|v| v.to_string())
                .unwrap_or_default()
        };
        let row = format!(
            "{:.0},{},{},{:.1},{},{},{:.2},{:.2},{},{},{:.0},{},{},{},{},{},{}",
            start.elapsed().as_secs_f64(),
            label.replace(',', " "),
            cstr(&s.capture_backend),
            s.frames_captured.saturating_sub(prev.frames_captured) as f64 / dt,
            s.fps_actual,
            s.capture_idle_ms,
            s.convert_ms,
            s.encode_ms_mean,
            s.encode_queue_drops.saturating_sub(prev.encode_queue_drops),
            s.idle_repeat_frames.saturating_sub(prev.idle_repeat_frames),
            bytes.saturating_sub(prev_bytes) as f64 * 8.0 / 1000.0 / dt,
            frames,
            fmt_p(50.0),
            fmt_p(95.0),
            fmt_p(99.0),
            s.capture_failed,
            cstr(&s.capture_history).replace(',', " "),
        );
        writeln!(file, "{row}").map_err(|e| e.to_string())?;
        println!("{row}");
        prev = s;
        prev_bytes = bytes;
    }

    let mut end: mello_sys::MelloStreamStats = unsafe { std::mem::zeroed() };
    unsafe { mello_sys::mello_stream_get_stats(host, &mut end) };
    let total = diff_hist(&end.present_delay_hist, &first_hist);
    let show = |p: f64| {
        histogram_percentile_ms(&total, p)
            .map(|v| format!("{v} ms"))
            .unwrap_or_else(|| "n/a".into())
    };
    println!(
        "Summary {label}: frames={} p50<={} p95<={} p99<={}",
        total.iter().sum::<u64>(),
        show(50.0),
        show(95.0),
        show(99.0)
    );

    unsafe { mello_sys::mello_stream_stop_host(host) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_bucket_upper_edge() {
        let mut h = [0u64; HIST_BUCKETS];
        h[3] = 90; // [3, 4) ms
        h[10] = 10; // [10, 11) ms
        assert_eq!(histogram_percentile_ms(&h, 50.0), Some(4));
        assert_eq!(histogram_percentile_ms(&h, 90.0), Some(4));
        assert_eq!(histogram_percentile_ms(&h, 95.0), Some(11));
    }

    #[test]
    fn empty_histogram_has_no_percentile() {
        assert_eq!(histogram_percentile_ms(&[0u64; HIST_BUCKETS], 95.0), None);
    }

    #[test]
    fn backend_names_parse() {
        assert_eq!(BenchBackend::parse("dxgi"), Some(BenchBackend::Dxgi));
        assert_eq!(
            BenchBackend::parse("wgc-window"),
            Some(BenchBackend::WgcWindow)
        );
        assert_eq!(BenchBackend::parse("nope"), None);
    }
}
