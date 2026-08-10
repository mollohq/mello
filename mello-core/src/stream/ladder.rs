//! Adaptive quality ladder — Stage 1: framerate rungs.
//!
//! Congestion control moves bitrate within a geometry fixed at stream start, so
//! when throughput falls the host keeps encoding 60fps and starves it. At
//! 1.5 Mbps a 720p60 stream gets ~0.027 bits per pixel per frame, roughly a
//! third of what the format needs: the encoder emits mush or misses the target
//! and stalls. Halving the framerate doubles the bits each frame receives.
//!
//! This is the ladder's first stage. Framerate rungs need no viewer-side change
//! at all — no SPS geometry change, no decoder re-init, no swap-chain resize.
//! Geometry rungs are Stage 2; see `plans/ADAPTIVE-QUALITY-LADDER.md`.

use std::time::{Duration, Instant};

/// Bits per pixel per frame below which the current framerate is unsustainable.
/// 720p60 at 3 Mbps sits at 0.054; the same 3 Mbps at 30fps is a comfortable
/// 0.108.
const DROP_BPP: f32 = 0.055;

/// Restore only once there is real headroom, not the moment we cross back over
/// the drop threshold — otherwise the stream oscillates across one boundary.
const RESTORE_BPP: f32 = 0.075;

/// Stalling is the worst outcome, so react fast on the way down.
const DROP_DWELL: Duration = Duration::from_secs(2);

/// Climb back slowly, matching the existing REMB policy where decreases apply
/// immediately and increases are rate-limited.
const RESTORE_DWELL: Duration = Duration::from_secs(15);

/// Minimum spacing between switches. Each one forces an IDR, so churn here is
/// itself a visible cost.
const SWITCH_COOLDOWN: Duration = Duration::from_secs(10);

/// Framerate below which there is no lower rung worth taking: past this, motion
/// is already poor and cutting further trades one artifact for a worse one.
const MIN_LADDER_FPS: u32 = 30;

/// Bits per pixel per frame at a given geometry and framerate.
///
/// The quality currency the ladder trades in: holding it roughly constant is
/// what keeps a picture looking the same as the geometry changes underneath it.
pub fn bits_per_pixel(bitrate_kbps: u32, width: u32, height: u32, fps: u32) -> f32 {
    if width == 0 || height == 0 || fps == 0 {
        return 0.0;
    }
    let pixels_per_sec = width as f64 * height as f64 * fps as f64;
    ((bitrate_kbps as f64 * 1000.0) / pixels_per_sec) as f32
}

/// Framerate rung controller.
///
/// Owns the encoder's framerate target. Congestion control feeds it a bitrate;
/// it decides the cadence that bitrate can actually sustain. The two must not
/// both drive the encoder, or they fight: dropping the rung lowers the required
/// bitrate, which would otherwise read as spare capacity and ramp straight back.
pub struct FramerateLadder {
    full_fps: u32,
    reduced_fps: u32,
    current_fps: u32,
    width: u32,
    height: u32,
    below_since: Option<Instant>,
    above_since: Option<Instant>,
    last_switch: Instant,
}

impl FramerateLadder {
    pub fn new(width: u32, height: u32, fps: u32, now: Instant) -> Self {
        Self {
            full_fps: fps,
            // Half rate, floored: 60->30, 30 stays 30 (no rung available).
            reduced_fps: (fps / 2).max(MIN_LADDER_FPS),
            current_fps: fps,
            width,
            height,
            below_since: None,
            above_since: None,
            last_switch: now,
        }
    }

    pub fn current_fps(&self) -> u32 {
        self.current_fps
    }

    /// True when this stream has a lower rung to take at all.
    pub fn has_rungs(&self) -> bool {
        self.full_fps > MIN_LADDER_FPS && self.reduced_fps < self.full_fps
    }

    /// Feed the current bitrate target. Returns the new framerate when the rung
    /// changes, otherwise `None`.
    ///
    /// Judged at *full* framerate in both directions, so the decision is about
    /// the stream's underlying capacity rather than the rung it currently sits
    /// on — otherwise dropping to 30fps would instantly double the measured bpp
    /// and argue for climbing straight back.
    pub fn observe(&mut self, bitrate_kbps: u32, now: Instant) -> Option<u32> {
        if !self.has_rungs() {
            return None;
        }

        let bpp_full = bits_per_pixel(bitrate_kbps, self.width, self.height, self.full_fps);
        let at_full = self.current_fps == self.full_fps;

        if at_full {
            self.above_since = None;
            if bpp_full < DROP_BPP {
                let since = *self.below_since.get_or_insert(now);
                if now.duration_since(since) >= DROP_DWELL
                    && now.duration_since(self.last_switch) >= SWITCH_COOLDOWN
                {
                    return Some(self.switch_to(self.reduced_fps, now));
                }
            } else {
                self.below_since = None;
            }
        } else {
            self.below_since = None;
            if bpp_full > RESTORE_BPP {
                let since = *self.above_since.get_or_insert(now);
                if now.duration_since(since) >= RESTORE_DWELL
                    && now.duration_since(self.last_switch) >= SWITCH_COOLDOWN
                {
                    return Some(self.switch_to(self.full_fps, now));
                }
            } else {
                self.above_since = None;
            }
        }
        None
    }

    fn switch_to(&mut self, fps: u32, now: Instant) -> u32 {
        self.current_fps = fps;
        self.last_switch = now;
        self.below_since = None;
        self.above_since = None;
        fps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder_720p60(now: Instant) -> FramerateLadder {
        FramerateLadder::new(1280, 720, 60, now)
    }

    /// The numbers the thresholds were chosen against, pinned so a later tweak
    /// has to be deliberate.
    #[test]
    fn bits_per_pixel_matches_the_ladder_table() {
        let bpp = |kbps, fps| (bits_per_pixel(kbps, 1280, 720, fps) * 1000.0).round() / 1000.0;
        assert_eq!(bpp(5000, 60), 0.090, "Medium at ceiling");
        assert_eq!(bpp(3000, 60), 0.054, "starved: below the drop threshold");
        assert_eq!(bpp(3000, 30), 0.109, "same bitrate, comfortable at 30fps");
    }

    #[test]
    fn degenerate_inputs_do_not_divide_by_zero() {
        assert_eq!(bits_per_pixel(5000, 0, 720, 60), 0.0);
        assert_eq!(bits_per_pixel(5000, 1280, 720, 0), 0.0);
    }

    /// A healthy stream must never leave the top rung.
    #[test]
    fn healthy_bitrate_holds_full_framerate() {
        let t0 = Instant::now();
        let mut ladder = ladder_720p60(t0);
        for s in 0..60 {
            let now = t0 + Duration::from_secs(s);
            assert_eq!(ladder.observe(5000, now), None);
        }
        assert_eq!(ladder.current_fps(), 60);
    }

    /// A brief dip must not switch: rung changes force an IDR, so reacting to
    /// every transient would itself damage the stream.
    #[test]
    fn transient_dip_does_not_switch() {
        let t0 = Instant::now();
        let mut ladder = ladder_720p60(t0);
        assert_eq!(ladder.observe(2000, t0 + Duration::from_millis(500)), None);
        assert_eq!(ladder.observe(5000, t0 + Duration::from_millis(1500)), None);
        assert_eq!(ladder.current_fps(), 60);
    }

    #[test]
    fn sustained_starvation_drops_to_half_framerate() {
        let t0 = Instant::now();
        let mut ladder = ladder_720p60(t0);
        // Cooldown is measured from construction, so the first switch cannot
        // land before it elapses.
        assert_eq!(ladder.observe(2500, t0 + Duration::from_secs(11)), None);
        let switched = ladder.observe(2500, t0 + Duration::from_secs(14));
        assert_eq!(switched, Some(30));
        assert_eq!(ladder.current_fps(), 30);
    }

    /// The failure this whole stage exists to prevent: after dropping to 30fps
    /// the same bitrate looks twice as good, and a naive controller reads that
    /// as spare capacity and climbs straight back into the starvation it just
    /// escaped.
    #[test]
    fn reduced_rung_does_not_immediately_climb_back() {
        let t0 = Instant::now();
        let mut ladder = ladder_720p60(t0);
        assert_eq!(ladder.observe(2500, t0 + Duration::from_secs(11)), None);
        assert_eq!(ladder.observe(2500, t0 + Duration::from_secs(14)), Some(30));

        // 2500kbps is 0.108 bpp at the rung we are now on — comfortable — but
        // only 0.054 at full framerate, which is what actually matters.
        for s in 15..90 {
            let now = t0 + Duration::from_secs(s);
            assert_eq!(
                ladder.observe(2500, now),
                None,
                "climbed back at t={s}s on unchanged bandwidth"
            );
        }
        assert_eq!(ladder.current_fps(), 30);
    }

    #[test]
    fn recovered_bandwidth_restores_full_framerate() {
        let t0 = Instant::now();
        let mut ladder = ladder_720p60(t0);
        assert_eq!(ladder.observe(2500, t0 + Duration::from_secs(11)), None);
        assert_eq!(ladder.observe(2500, t0 + Duration::from_secs(14)), Some(30));

        // 4500kbps = 0.081 bpp at 60fps, above the restore threshold.
        let mut restored = None;
        for s in 15..60 {
            let now = t0 + Duration::from_secs(s);
            if let Some(fps) = ladder.observe(4500, now) {
                restored = Some((fps, s));
                break;
            }
        }
        let (fps, at) = restored.expect("never restored despite sustained headroom");
        assert_eq!(fps, 60);
        assert!(at >= 30, "restored after {at}s, faster than the 15s dwell");
    }

    /// Between the drop and restore thresholds nothing should happen, in either
    /// direction — that gap is what stops the stream oscillating.
    #[test]
    fn bitrate_inside_the_hysteresis_band_is_stable() {
        let t0 = Instant::now();
        let mut ladder = ladder_720p60(t0);
        // 3500kbps = 0.063 bpp: above DROP, below RESTORE.
        for s in 0..90 {
            assert_eq!(ladder.observe(3500, t0 + Duration::from_secs(s)), None);
        }
        assert_eq!(ladder.current_fps(), 60);
    }

    /// A 30fps preset has nowhere lower to go, so the ladder must stay inert
    /// rather than cutting to something unwatchable.
    #[test]
    fn a_30fps_preset_has_no_rung_to_drop_to() {
        let t0 = Instant::now();
        let mut ladder = FramerateLadder::new(1280, 720, 30, t0);
        assert!(!ladder.has_rungs());
        for s in 0..60 {
            assert_eq!(ladder.observe(500, t0 + Duration::from_secs(s)), None);
        }
        assert_eq!(ladder.current_fps(), 30);
    }
}
