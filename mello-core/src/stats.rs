//! Runtime stats snapshot for debug UI and perf harness (spec 15).

use serde::{Deserialize, Serialize};

/// Lightweight process stats refreshed on a 1s tick.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MelloStats {
    pub nakama_connected: bool,
    pub voice_active: bool,
    pub stream_hosting: bool,
    pub stream_watching: bool,
    /// Resident set size in megabytes (best-effort; 0 if unavailable).
    pub process_rss_mb: u32,
    /// Physical footprint in megabytes (macOS `phys_footprint` — the number
    /// Activity Monitor shows as "Memory"). Best-effort; 0 if unavailable.
    #[serde(default)]
    pub process_footprint_mb: u32,
}

/// Best-effort resident set size for the current process.
pub fn process_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        linux_rss_bytes()
    }
    #[cfg(target_os = "macos")]
    {
        macos_rss_bytes()
    }
    #[cfg(target_os = "windows")]
    {
        windows_rss_bytes()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn linux_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn macos_rss_bytes() -> Option<u64> {
    mach_rss_bytes().or_else(rss_via_ps)
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn mach_rss_bytes() -> Option<u64> {
    use std::mem::MaybeUninit;

    unsafe {
        let mut info = MaybeUninit::<libc::mach_task_basic_info>::uninit();
        let mut count = (std::mem::size_of::<libc::mach_task_basic_info>()
            / std::mem::size_of::<libc::natural_t>())
            as libc::mach_msg_type_number_t;
        let kret = libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr() as *mut libc::integer_t,
            &mut count,
        );
        if kret != libc::KERN_SUCCESS {
            return None;
        }
        Some(info.assume_init().resident_size)
    }
}

#[cfg(target_os = "macos")]
fn rss_via_ps() -> Option<u64> {
    let pid = std::process::id();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let kb: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(kb * 1024)
}

#[cfg(target_os = "windows")]
fn windows_rss_bytes() -> Option<u64> {
    use std::mem::MaybeUninit;

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "psapi")]
    extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            ppsmem_counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    unsafe {
        let mut counters = MaybeUninit::<ProcessMemoryCounters>::uninit();
        let ok = GetProcessMemoryInfo(
            GetCurrentProcess(),
            counters.as_mut_ptr(),
            std::mem::size_of::<ProcessMemoryCounters>() as u32,
        );
        if ok == 0 {
            return None;
        }
        Some(counters.assume_init().working_set_size as u64)
    }
}

pub fn rss_to_mb(bytes: u64) -> u32 {
    (bytes / (1024 * 1024)) as u32
}

/// Per-process resource usage sampled via `proc_pid_rusage` (macOS). This is the
/// source for the two metrics that actually matter for "beat Discord": macOS
/// `phys_footprint` (not `ps rss`, which over-counts shared pages) and wakeups.
/// Works for the current process or any child PID (same user).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcRusage {
    /// macOS `phys_footprint` in bytes (Activity Monitor "Memory").
    pub phys_footprint_bytes: u64,
    /// Package idle wakeups, cumulative since process start.
    pub pkg_idle_wakeups: u64,
    /// Interrupt wakeups, cumulative since process start.
    pub interrupt_wakeups: u64,
    /// User CPU time in nanoseconds, cumulative.
    pub user_time_ns: u64,
    /// System CPU time in nanoseconds, cumulative.
    pub system_time_ns: u64,
}

/// Sample `proc_pid_rusage` for `pid`. macOS only; `None` elsewhere or on error.
#[cfg(target_os = "macos")]
pub fn proc_rusage(pid: u32) -> Option<ProcRusage> {
    use std::mem::MaybeUninit;

    // `RUSAGE_INFO_V4`. We only read fields that have existed since v0 (their
    // offsets are stable across every flavor); `_tail` pads the struct well past
    // the size of the newest `rusage_info` flavor so the kernel write can never
    // overflow our buffer even if the OS fills a larger struct than requested.
    const RUSAGE_INFO_V4: libc::c_int = 4;

    #[repr(C)]
    struct RusageInfoV4 {
        ri_uuid: [u8; 16],
        ri_user_time: u64,
        ri_system_time: u64,
        ri_pkg_idle_wkups: u64,
        ri_interrupt_wkups: u64,
        ri_pageins: u64,
        ri_wired_size: u64,
        ri_resident_size: u64,
        ri_phys_footprint: u64,
        _tail: [u64; 48],
    }

    extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }

    unsafe {
        let mut info = MaybeUninit::<RusageInfoV4>::zeroed();
        let rc = proc_pid_rusage(
            pid as libc::c_int,
            RUSAGE_INFO_V4,
            info.as_mut_ptr() as *mut libc::c_void,
        );
        if rc != 0 {
            return None;
        }
        let info = info.assume_init();
        Some(ProcRusage {
            phys_footprint_bytes: info.ri_phys_footprint,
            pkg_idle_wakeups: info.ri_pkg_idle_wkups,
            interrupt_wakeups: info.ri_interrupt_wkups,
            user_time_ns: info.ri_user_time,
            system_time_ns: info.ri_system_time,
        })
    }
}

/// Non-macOS stub — footprint/wakeups sampling is macOS-only for now.
#[cfg(not(target_os = "macos"))]
pub fn proc_rusage(_pid: u32) -> Option<ProcRusage> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_rss_bytes_nonzero_on_host() {
        let rss = process_rss_bytes().unwrap_or(0);
        assert!(rss > 0, "expected non-zero RSS on this platform");
    }

    #[test]
    fn proc_rusage_reports_footprint_for_self() {
        // macOS-only; elsewhere `proc_rusage` returns None and this is a no-op.
        if let Some(r) = proc_rusage(std::process::id()) {
            assert!(r.phys_footprint_bytes > 0, "expected non-zero footprint");
        }
    }
}
