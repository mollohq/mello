//! The load guard: how much CPU other processes used during a run.
//!
//! The gate runs in real time. A build or a game on the same machine
//! starves the encoder, the pacer and the viewer tick, and every metric
//! moves with it (measured 2026-10-21: two `cargo clippy` runs of other
//! worktrees took the CPU lane's clean profile from 48 to 13-26 fps). A run
//! on a busy machine says nothing about the code, so the harness measures
//! the load and calls such a run invalid instead of failing or passing it.
//!
//! Other load = system busy time (`GetSystemTimes`) minus the CPU time of
//! this process and of the SFU process (`GetProcessTimes`), as a share of
//! all logical processors over the run.

/// Over this average share of all logical processors, other processes made
/// the run invalid. A quiet machine measures 0 to 3 %.
pub const MAX_OTHER_PCT: f64 = 10.0;

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        lo: u32,
        hi: u32,
    }

    impl FileTime {
        fn ticks(self) -> u64 {
            (u64::from(self.hi) << 32) | u64::from(self.lo)
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
    }

    /// System idle and total time, and the CPU time of the given processes
    /// (this process is always counted), in 100 ns units.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Sample {
        idle: u64,
        total: u64,
        own: u64,
    }

    fn process_ticks(h: *mut c_void) -> u64 {
        let (mut c, mut e, mut k, mut u) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        // SAFETY: valid out pointers; a stale handle makes the call fail.
        if unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } == 0 {
            return 0;
        }
        k.ticks() + u.ticks()
    }

    pub fn sample(others: &[*mut c_void]) -> Sample {
        let (mut i, mut k, mut u) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        // SAFETY: valid out pointers.
        if unsafe { GetSystemTimes(&mut i, &mut k, &mut u) } == 0 {
            return Sample::default();
        }
        // SAFETY: the pseudo handle of this process.
        let mut own = process_ticks(unsafe { GetCurrentProcess() });
        for &h in others {
            own += process_ticks(h);
        }
        Sample {
            idle: i.ticks(),
            // Kernel time includes idle time.
            total: k.ticks() + u.ticks(),
            own,
        }
    }

    /// Share of all logical processors that other processes used between
    /// two samples, in percent. None when the counters did not move.
    pub fn other_pct(a: Sample, b: Sample) -> Option<f64> {
        let total = b.total.checked_sub(a.total)?;
        if total == 0 {
            return None;
        }
        let busy = total.saturating_sub(b.idle.saturating_sub(a.idle));
        let own = b.own.saturating_sub(a.own);
        Some(100.0 * busy.saturating_sub(own) as f64 / total as f64)
    }
}

#[cfg(not(windows))]
mod imp {
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Sample;
    pub fn sample(_: &[*mut std::ffi::c_void]) -> Sample {
        Sample
    }
    pub fn other_pct(_: Sample, _: Sample) -> Option<f64> {
        None
    }
}

pub use imp::{other_pct, sample, Sample};

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn this_process_does_not_count_as_other_load() {
        let a = sample(&[]);
        // Busy work in this process only.
        let t = std::time::Instant::now();
        let mut x = 0u64;
        while t.elapsed() < std::time::Duration::from_millis(300) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let b = sample(&[]);
        let pct = other_pct(a, b).expect("counters move");
        // One busy core of this process is about 100 / cores percent of the
        // machine; none of it may show as other load. Other processes on a
        // developer machine may still use a little.
        assert!(pct < 50.0, "{pct}");
    }
}
