//! The quality controller: the one owner of the stream host's
//! `(bitrate, fps, resolution)` (streaming-quality stage 4).
//!
//! Inputs:
//!
//! * the goog_cc target of the host-to-SFU leg (`GCC_TARGET` feedback, the
//!   encoder share of the target),
//! * the forwarded minimum viewer REMB, as a ceiling,
//! * encoder overload (encode-queue drops and the mean encode time),
//! * the source size (stage 8 stats), which sets the geometry of the rungs,
//! * the viewer count (the manager clears the REMB ceiling at zero viewers).
//!
//! Outputs: the encoder bitrate, the rung (`fps` and the encode size) and the
//! encoder allocation that the sender gives goog_cc.
//!
//! Rungs: the preset, then 60 to 30 fps, then 540p, 480p and 360p at 30 fps,
//! judged in bits per pixel per frame. The geometry rungs are off by default
//! (`GEOMETRY_RUNGS_DEFAULT`) until the viewers handle a size change. Down after 2 s under the rung's floor
//! (or 2 s of encoder overload), up after 15 s over the next rung's restore
//! point, 10 s between two switches, and no wait before the first switch.
//! The bitrate starts at 60 % of the preset and follows the inputs with the
//! stage 2b coalescing rule: a change inside a 5 % dead band does nothing, a
//! decrease over it applies at once, an increase waits 250 ms after the last
//! change. Floor 300 kbps.

use std::time::{Duration, Instant};

/// The lowest encoder bitrate the controller sets.
pub const FLOOR_KBPS: u32 = 300;
/// The encoder starts at this share of the preset bitrate; goog_cc's probes
/// take it to the target (handoff section 4: 0.12 s in the spike).
pub const START_PERCENT: u32 = 60;
/// RTP, UDP and IP headers over the H.264 bitrate (`pacer.rs`). goog_cc's
/// target counts them, the encoder bitrate does not.
pub const RTP_OVERHEAD_PERCENT: u32 = 108;

/// Bits per pixel per frame under which a rung cannot hold its picture:
/// 720p60 at 3 Mbps is 0.054.
const DROP_BPP: f32 = 0.055;
/// Bits per pixel per frame over which the rung above holds its picture.
const RESTORE_BPP: f32 = 0.075;
/// A preset whose own bitrate is under its restore point (1080p60 at
/// 8 Mbps is 0.064 bpp) restores at this share of the preset bitrate, or a
/// stream that dropped to 30 fps would never go back to 60.
const RESTORE_PRESET_PERCENT: u32 = 95;
const DOWN_DWELL: Duration = Duration::from_secs(2);
const UP_DWELL: Duration = Duration::from_secs(15);
const SWITCH_COOLDOWN: Duration = Duration::from_secs(10);
/// The geometry rungs, at `LOW_FPS`.
const GEOMETRY_HEIGHTS: [u32; 3] = [540, 480, 360];
/// The geometry rungs are off by default: a viewer does not handle a size
/// change yet. Its software decoder (OpenH264) copies the configured size
/// out of a smaller decoded picture and crashes (the stage 4 gate, cpu lane,
/// congested: exit 139 at the step to 540p), and its D3D11 present shows the
/// smaller picture in a corner of the configured size. The encoder side
/// (`mello_stream_set_resolution`) is in and tested. They come on when the
/// viewer handles a new size; `MELLO_STREAM_GEOMETRY_RUNGS=1` turns them on
/// for that work and the gate.
pub const GEOMETRY_RUNGS_DEFAULT: bool = false;
const GEOMETRY_RUNGS_ENV: &str = "MELLO_STREAM_GEOMETRY_RUNGS";
const LOW_FPS: u32 = 30;

/// The stage 2b coalescing rule (manager.rs before stage 4).
pub const BITRATE_CHANGE_MIN_INTERVAL: Duration = Duration::from_millis(250);
pub const BITRATE_DEAD_BAND_PERCENT: u64 = 5;

/// Encoder overload over one window: more than 2 % of the offered frames
/// dropped by the encode queue, or a mean encode time at or over 80 % of the
/// frame interval (the thresholds of `VideoPipeline::encoder_is_overloaded`).
const OVERLOAD_DROP_SHARE: f64 = 0.02;
const OVERLOAD_BUDGET_SHARE: f64 = 0.80;
/// A window with fewer offered frames tells nothing.
const OVERLOAD_MIN_FRAMES: u64 = 30;

/// Control-message subtype of the rung message, in the `0x04` family of the
/// pause (`0x03`), idle (`0x04`) and cursor (`0x02`) messages:
/// `[0x04, 0x05, width: u16 BE, height: u16 BE, fps]`.
pub const RUNG_MESSAGE_TYPE: u8 = 0x04;
pub const RUNG_MESSAGE_SUBTYPE: u8 = 0x05;

/// One rung: the encode size and the frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rung {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Rung {
    fn pixel_rate(&self) -> f64 {
        f64::from(self.width) * f64::from(self.height) * f64::from(self.fps)
    }

    /// Bits per pixel per frame at `kbps`.
    pub fn bits_per_pixel(&self, kbps: u32) -> f32 {
        let rate = self.pixel_rate();
        if rate <= 0.0 {
            return 0.0;
        }
        (f64::from(kbps) * 1000.0 / rate) as f32
    }

    fn kbps_at_bpp(&self, bpp: f32) -> u32 {
        (self.pixel_rate() * f64::from(bpp) / 1000.0).round() as u32
    }
}

/// The rung message the host sends the viewers on every rung change.
pub fn rung_message(rung: Rung) -> [u8; 7] {
    let w = u16::try_from(rung.width).unwrap_or(u16::MAX).to_be_bytes();
    let h = u16::try_from(rung.height).unwrap_or(u16::MAX).to_be_bytes();
    let fps = u8::try_from(rung.fps).unwrap_or(u8::MAX);
    [
        RUNG_MESSAGE_TYPE,
        RUNG_MESSAGE_SUBTYPE,
        w[0],
        w[1],
        h[0],
        h[1],
        fps,
    ]
}

/// Parses a rung message; `None` for any other control message.
pub fn parse_rung_message(data: &[u8]) -> Option<Rung> {
    if data.len() != 7 || data[0] != RUNG_MESSAGE_TYPE || data[1] != RUNG_MESSAGE_SUBTYPE {
        return None;
    }
    Some(Rung {
        width: u32::from(u16::from_be_bytes([data[2], data[3]])),
        height: u32::from(u16::from_be_bytes([data[4], data[5]])),
        fps: u32::from(data[6]),
    })
}

/// The width that keeps the aspect of `width` x `height` at `target_height`,
/// rounded to a multiple of 16 (as libmello's `encode_size_for`).
fn width_for(width: u32, height: u32, target_height: u32) -> u32 {
    if height == 0 || target_height >= height {
        return width;
    }
    let exact = f64::from(width) * f64::from(target_height) / f64::from(height);
    let mut w = ((exact / 16.0).round() as u32) * 16;
    if w > width {
        w = width & !15;
    }
    if w < 16 {
        w = width.min(16);
    }
    w
}

/// H.264 level 5.1 limits, as libmello's `encode_size_for`.
const LEVEL51_MAX_FRAME_MBS: u64 = 36_864;
const LEVEL51_MAX_MBS_PER_SEC: u64 = 983_040;

/// The encode size of a source at a preset height: libmello's
/// `VideoPipeline::encode_size_for` (the preset height, the width from the
/// source aspect rounded to 16, never over the source, the height lowered
/// until the size fits H.264 level 5.1). The top rung after a source size
/// change.
pub fn encode_size_for(source_w: u32, source_h: u32, preset_h: u32, fps: u32) -> (u32, u32) {
    let cap_w = source_w & !1;
    let cap_h = source_h & !1;
    if cap_w == 0 || cap_h == 0 {
        return (0, 0);
    }
    let fits = |w: u32, h: u32| {
        let mbs = u64::from(w.div_ceil(16)) * u64::from(h.div_ceil(16));
        mbs <= LEVEL51_MAX_FRAME_MBS && mbs * u64::from(fps) <= LEVEL51_MAX_MBS_PER_SEC
    };
    let mut h = if preset_h > 0 && preset_h < cap_h {
        preset_h & !1
    } else {
        cap_h
    };
    let mut w = width_for(cap_w, cap_h, h);
    while h > 2 && !fits(w, h) {
        h -= 2;
        w = width_for(cap_w, cap_h, h);
    }
    (w, h)
}

/// The rungs of a stream whose top rung is `width` x `height` at `fps`.
/// The geometry rungs on or off: `MELLO_STREAM_GEOMETRY_RUNGS` (`1` or `0`),
/// else `GEOMETRY_RUNGS_DEFAULT`.
pub fn geometry_rungs_enabled() -> bool {
    match std::env::var(GEOMETRY_RUNGS_ENV) {
        Ok(v) if v == "1" => true,
        Ok(v) if v == "0" => false,
        _ => GEOMETRY_RUNGS_DEFAULT,
    }
}

/// The rungs of a preset: `rungs_for`, without the geometry rungs when they
/// are off.
fn ladder(width: u32, height: u32, fps: u32, geometry: bool) -> Vec<Rung> {
    let mut rungs = rungs_for(width, height, fps);
    if !geometry {
        rungs.retain(|r| r.width == width && r.height == height);
    }
    rungs
}

pub fn rungs_for(width: u32, height: u32, fps: u32) -> Vec<Rung> {
    let mut rungs = vec![Rung { width, height, fps }];
    if fps > LOW_FPS {
        rungs.push(Rung {
            width,
            height,
            fps: LOW_FPS,
        });
    }
    for target in GEOMETRY_HEIGHTS {
        if target < height {
            rungs.push(Rung {
                width: width_for(width, height, target),
                height: target,
                fps: LOW_FPS.min(fps),
            });
        }
    }
    rungs
}

/// One window of encoder load, from `MelloStreamStats` deltas.
#[derive(Debug, Clone, Copy, Default)]
pub struct EncoderLoad {
    pub frames_offered: u64,
    pub queue_drops: u64,
    pub mean_encode_ms: f64,
    pub fps: u32,
}

impl EncoderLoad {
    /// True when the encoder does not hold the frame budget.
    pub fn overloaded(&self) -> bool {
        if self.frames_offered < OVERLOAD_MIN_FRAMES || self.fps == 0 {
            return false;
        }
        let drop_share = self.queue_drops as f64 / self.frames_offered as f64;
        let budget_ms = 1000.0 / f64::from(self.fps);
        drop_share > OVERLOAD_DROP_SHARE || self.mean_encode_ms >= OVERLOAD_BUDGET_SHARE * budget_ms
    }
}

/// What the manager must apply after an input or a tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Update {
    /// The new encoder bitrate.
    pub bitrate_kbps: Option<u32>,
    /// The new rung (frame rate and encode size).
    pub rung: Option<Rung>,
    /// The new encoder allocation for goog_cc, in kbps with the RTP
    /// overhead. A raise makes the sender re-probe.
    pub allocation_kbps: Option<u32>,
}

/// See the module documentation.
pub struct QualityController {
    preset_kbps: u32,
    rungs: Vec<Rung>,
    rung: usize,
    /// The geometry rungs are in `rungs` (`geometry_rungs_enabled`).
    geometry: bool,
    /// Encoder share of the goog_cc target, kbps.
    gcc_kbps: Option<u32>,
    remb_kbps: Option<u32>,
    overloaded: bool,
    bitrate_kbps: u32,
    last_bitrate_change: Option<Instant>,
    below_since: Option<Instant>,
    above_since: Option<Instant>,
    last_switch: Option<Instant>,
}

impl QualityController {
    /// `width` x `height` is the encode size at the preset (libmello's
    /// `encode_size_for`), `fps` and `preset_kbps` the preset's.
    pub fn new(width: u32, height: u32, fps: u32, preset_kbps: u32) -> Self {
        let preset_kbps = preset_kbps.max(FLOOR_KBPS);
        let geometry = geometry_rungs_enabled();
        Self {
            preset_kbps,
            rungs: ladder(width, height, fps, geometry),
            rung: 0,
            geometry,
            gcc_kbps: None,
            remb_kbps: None,
            overloaded: false,
            bitrate_kbps: (preset_kbps * START_PERCENT / 100).max(FLOOR_KBPS),
            last_bitrate_change: None,
            below_since: None,
            above_since: None,
            last_switch: None,
        }
    }

    /// Turns the geometry rungs on or off (tests; the default comes from
    /// `geometry_rungs_enabled`).
    pub fn with_geometry_rungs(mut self, on: bool) -> Self {
        let top = self.rungs[0];
        self.geometry = on;
        self.rungs = ladder(top.width, top.height, top.fps, on);
        self.rung = self.rung.min(self.rungs.len() - 1);
        self
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    pub fn rung(&self) -> Rung {
        self.rungs[self.rung]
    }

    pub fn rung_index(&self) -> usize {
        self.rung
    }

    pub fn rungs(&self) -> &[Rung] {
        &self.rungs
    }

    pub fn preset_kbps(&self) -> u32 {
        self.preset_kbps
    }

    /// goog_cc limits for the sender, kbps with the RTP overhead: floor,
    /// start and preset.
    pub fn sender_limits_kbps(&self) -> (u32, u32, u32) {
        (
            with_overhead(FLOOR_KBPS),
            with_overhead(self.bitrate_kbps),
            with_overhead(self.preset_kbps),
        )
    }

    /// The encoder allocation of the current rung, kbps with the RTP
    /// overhead: the preset on the top rung, else the bitrate at which the
    /// rung above restores (the most this rung needs before a step up).
    pub fn allocation_kbps(&self) -> u32 {
        if self.rung == 0 {
            return with_overhead(self.preset_kbps);
        }
        with_overhead(self.restore_kbps(self.rung - 1))
    }

    /// The goog_cc encoder target in bps, with the RTP overhead.
    pub fn set_gcc_target_bps(&mut self, bps: u32) {
        self.gcc_kbps = Some(without_overhead(bps / 1000));
    }

    /// The fresh minimum viewer REMB, or `None` when there is none.
    pub fn set_remb_ceiling_kbps(&mut self, kbps: Option<u32>) {
        self.remb_kbps = kbps;
    }

    pub fn set_encoder_load(&mut self, load: EncoderLoad) {
        self.overloaded = load.overloaded();
    }

    /// A new source size: the rungs follow its aspect at the preset height.
    /// Returns the current rung when its geometry changed.
    pub fn set_top_size(&mut self, width: u32, height: u32) -> Option<Rung> {
        let top = self.rungs[0];
        if width == 0 || height == 0 || (width == top.width && height == top.height) {
            return None;
        }
        let before = self.rungs[self.rung];
        self.rungs = ladder(width, height, top.fps, self.geometry);
        self.rung = self.rung.min(self.rungs.len() - 1);
        let after = self.rungs[self.rung];
        (after != before).then_some(after)
    }

    /// The bitrate the inputs allow: the goog_cc share, under the REMB
    /// ceiling and the preset, over the floor.
    pub fn desired_kbps(&self) -> u32 {
        let mut kbps = self.preset_kbps;
        if let Some(gcc) = self.gcc_kbps {
            kbps = kbps.min(gcc);
        }
        if let Some(remb) = self.remb_kbps {
            kbps = kbps.min(remb);
        }
        kbps.max(FLOOR_KBPS)
    }

    fn outside_dead_band(&self, kbps: u32) -> bool {
        let current = u64::from(self.bitrate_kbps);
        u64::from(kbps.abs_diff(self.bitrate_kbps)) * 100 >= current * BITRATE_DEAD_BAND_PERCENT
    }

    /// Applies the coalescing rule to the desired bitrate. Returns the new
    /// bitrate when it changes. Call it on every input and on a short tick,
    /// so a held increase applies when its 250 ms window ends.
    pub fn update_bitrate(&mut self, now: Instant) -> Option<u32> {
        let desired = self.desired_kbps();
        if desired == self.bitrate_kbps {
            return None;
        }
        if desired < self.bitrate_kbps {
            // A decrease over the dead band applies at once: the link
            // loses packets now. A smaller one changes nothing.
            if !self.outside_dead_band(desired) {
                return None;
            }
        } else {
            if let Some(last) = self.last_bitrate_change {
                if now.duration_since(last) < BITRATE_CHANGE_MIN_INTERVAL {
                    return None;
                }
            }
            // The last step to the preset can be under the dead band;
            // without it the stream stays up to 5 % under the preset.
            if !self.outside_dead_band(desired) && desired != self.preset_kbps {
                return None;
            }
        }
        self.bitrate_kbps = desired;
        self.last_bitrate_change = Some(now);
        Some(desired)
    }

    /// The floor of rung `index`, kbps.
    fn drop_kbps(&self, index: usize) -> u32 {
        self.rungs[index].kbps_at_bpp(DROP_BPP)
    }

    /// The bitrate over which rung `index` restores, kbps.
    fn restore_kbps(&self, index: usize) -> u32 {
        let by_bpp = self.rungs[index].kbps_at_bpp(RESTORE_BPP);
        let by_preset = self.preset_kbps * RESTORE_PRESET_PERCENT / 100;
        // Keep a gap over the floor so a restore cannot drop back at once.
        by_bpp.min(by_preset).max(self.drop_kbps(index) * 11 / 10)
    }

    fn cooled_down(&self, now: Instant) -> bool {
        self.last_switch
            .map(|at| now.duration_since(at) >= SWITCH_COOLDOWN)
            .unwrap_or(true)
    }

    /// Judges the rung on the applied bitrate and the encoder load. Returns
    /// the update to apply when the rung changes. Call it once a second.
    pub fn tick_rung(&mut self, now: Instant) -> Update {
        let down = self.rung + 1 < self.rungs.len()
            && (self.bitrate_kbps < self.drop_kbps(self.rung) || self.overloaded);
        let up = self.rung > 0
            && !self.overloaded
            && self.bitrate_kbps >= self.restore_kbps(self.rung - 1);

        if down {
            self.above_since = None;
            let since = *self.below_since.get_or_insert(now);
            if now.duration_since(since) >= DOWN_DWELL && self.cooled_down(now) {
                return self.switch_to(self.rung + 1, now);
            }
            return Update::default();
        }
        self.below_since = None;
        if up {
            let since = *self.above_since.get_or_insert(now);
            if now.duration_since(since) >= UP_DWELL && self.cooled_down(now) {
                return self.switch_to(self.rung - 1, now);
            }
            return Update::default();
        }
        self.above_since = None;
        Update::default()
    }

    /// Test seam: the bitrate the encoder runs at and its last change.
    #[cfg(test)]
    pub(crate) fn set_applied_bitrate(&mut self, kbps: u32, changed_at: Option<Instant>) {
        self.bitrate_kbps = kbps;
        self.last_bitrate_change = changed_at;
    }

    fn switch_to(&mut self, index: usize, now: Instant) -> Update {
        self.rung = index;
        self.last_switch = Some(now);
        self.below_since = None;
        self.above_since = None;
        Update {
            bitrate_kbps: None,
            rung: Some(self.rungs[index]),
            allocation_kbps: Some(self.allocation_kbps()),
        }
    }
}

fn with_overhead(kbps: u32) -> u32 {
    (u64::from(kbps) * u64::from(RTP_OVERHEAD_PERCENT) / 100) as u32
}

fn without_overhead(kbps: u32) -> u32 {
    (u64::from(kbps) * 100 / u64::from(RTP_OVERHEAD_PERCENT)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ultra(now: Instant) -> (QualityController, Instant) {
        (
            QualityController::new(1920, 1080, 60, 8_000).with_geometry_rungs(true),
            now,
        )
    }

    fn gcc_kbps(controller: &mut QualityController, kbps: u32) {
        controller.set_gcc_target_bps(with_overhead(kbps) * 1000);
    }

    /// Feeds the goog_cc target and ticks once a second for `secs`.
    fn run(
        controller: &mut QualityController,
        start: Instant,
        secs: u64,
        kbps: u32,
    ) -> Vec<Update> {
        let mut updates = Vec::new();
        for s in 0..=secs {
            let now = start + Duration::from_secs(s);
            gcc_kbps(controller, kbps);
            controller.update_bitrate(now);
            let update = controller.tick_rung(now);
            if update.rung.is_some() {
                updates.push(update);
            }
        }
        updates
    }

    #[test]
    fn the_rungs_are_the_preset_then_30_fps_then_540_480_360() {
        let rungs = rungs_for(1920, 1080, 60);
        let shapes: Vec<(u32, u32, u32)> =
            rungs.iter().map(|r| (r.width, r.height, r.fps)).collect();
        assert_eq!(
            shapes,
            vec![
                (1920, 1080, 60),
                (1920, 1080, 30),
                (960, 540, 30),
                (848, 480, 30),
                (640, 360, 30)
            ]
        );
        // A 30 fps preset has no frame-rate rung; a 480p preset only 360p.
        assert_eq!(rungs_for(1920, 1080, 30).len(), 4);
        assert_eq!(rungs_for(848, 480, 30).len(), 2);
        // The geometry follows the encode aspect (ultrawide 2288x960).
        assert_eq!(
            rungs_for(2288, 960, 60)[2],
            Rung {
                width: 1280,
                height: 540,
                fps: 30
            }
        );
    }

    #[test]
    fn the_bitrate_starts_at_60_percent_of_the_preset() {
        let (controller, _) = ultra(Instant::now());
        assert_eq!(controller.bitrate_kbps(), 4_800);
        let (floor, start, max) = controller.sender_limits_kbps();
        assert_eq!((floor, start, max), (324, 5_184, 8_640));
    }

    #[test]
    fn the_gcc_target_drives_the_encoder_and_the_preset_caps_it() {
        let (mut controller, now) = ultra(Instant::now());
        gcc_kbps(&mut controller, 20_000);
        assert_eq!(controller.update_bitrate(now), Some(8_000));
        gcc_kbps(&mut controller, 3_000);
        assert_eq!(controller.update_bitrate(now), Some(3_000));
        gcc_kbps(&mut controller, 100);
        assert_eq!(controller.update_bitrate(now), Some(FLOOR_KBPS));
    }

    #[test]
    fn the_remb_ceiling_caps_the_gcc_target() {
        let (mut controller, now) = ultra(Instant::now());
        gcc_kbps(&mut controller, 8_000);
        controller.set_remb_ceiling_kbps(Some(2_500));
        assert_eq!(controller.update_bitrate(now), Some(2_500));
        controller.set_remb_ceiling_kbps(None);
        let later = now + Duration::from_millis(300);
        assert_eq!(controller.update_bitrate(later), Some(8_000));
    }

    // The stage 2b rule, kept: a decrease inside the dead band changes
    // nothing, a decrease over it applies at once even right after an
    // increase, and an increase waits 250 ms after the last change.
    #[test]
    fn bitrate_changes_follow_the_coalescing_rule() {
        let (mut controller, now) = ultra(Instant::now());
        gcc_kbps(&mut controller, 4_700); // -2 %
        assert_eq!(controller.update_bitrate(now), None);
        gcc_kbps(&mut controller, 6_000);
        assert_eq!(controller.update_bitrate(now), Some(6_000));
        gcc_kbps(&mut controller, 7_000);
        assert_eq!(
            controller.update_bitrate(now + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            controller.update_bitrate(now + Duration::from_millis(260)),
            Some(7_000)
        );
        gcc_kbps(&mut controller, 5_000);
        assert_eq!(
            controller.update_bitrate(now + Duration::from_millis(270)),
            Some(5_000)
        );
    }

    // A ramp of small goog_cc steps reaches the encoder at most every 250 ms
    // (manager.rs `remb_ramp_changes_the_encoder_at_most_once_per_250_ms`).
    #[test]
    fn a_ramp_changes_the_encoder_at_most_once_per_250_ms() {
        let (mut controller, start) = ultra(Instant::now());
        let mut changes = 0;
        for ms in (0..3_000).step_by(10) {
            gcc_kbps(&mut controller, 4_800 + ms as u32);
            if controller
                .update_bitrate(start + Duration::from_millis(ms))
                .is_some()
            {
                changes += 1;
            }
        }
        assert!(changes <= 12, "{changes} changes in 3 s");
        assert!(changes >= 5, "{changes} changes in 3 s");
    }

    // The stage 4 gate: at a cap of 60 % of the 1080p60 preset the stream
    // must keep at least 30 fps. 4.8 Mbps is 0.039 bpp at 60 fps: the
    // controller drops to 30 fps after 2 s, with no wait at the start.
    #[test]
    fn a_cap_at_60_percent_drops_to_30_fps_in_2_seconds() {
        let (mut controller, start) = ultra(Instant::now());
        let updates = run(&mut controller, start, 5, 4_800);
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].rung,
            Some(Rung {
                width: 1920,
                height: 1080,
                fps: 30
            })
        );
        assert_eq!(controller.rung_index(), 1);
    }

    #[test]
    fn a_deeper_cap_steps_down_to_the_geometry_rungs_10_s_apart() {
        let (mut controller, start) = ultra(Instant::now());
        // 600 kbps: 0.0096 bpp at 1080p30, 0.039 at 540p30, 0.049 at 480p30,
        // 0.087 at 360p30.
        let updates = run(&mut controller, start, 40, 600);
        let heights: Vec<u32> = updates
            .iter()
            .map(|u| u.rung.expect("rung").height)
            .collect();
        assert_eq!(heights, vec![1080, 540, 480, 360]);
        assert_eq!(controller.rung().height, 360);
    }

    /// The default ladder (geometry rungs off) stops at 30 fps: a deep cap
    /// keeps the preset size, so no viewer sees a size change.
    #[test]
    fn without_the_geometry_rungs_a_deep_cap_keeps_the_preset_size() {
        let mut controller =
            QualityController::new(1920, 1080, 60, 8_000).with_geometry_rungs(false);
        assert_eq!(controller.rungs().len(), 2);
        let updates = run(&mut controller, Instant::now(), 40, 600);
        assert!(updates
            .iter()
            .filter_map(|u| u.rung)
            .all(|r| r.width == 1920 && r.height == 1080));
        assert_eq!(
            controller.rung(),
            Rung {
                width: 1920,
                height: 1080,
                fps: 30
            }
        );
    }

    #[test]
    fn the_rung_comes_back_after_15_s_of_headroom() {
        let (mut controller, start) = ultra(Instant::now());
        run(&mut controller, start, 3, 4_800);
        assert_eq!(controller.rung().fps, 30);
        // At the preset bitrate 1080p60 is 0.064 bpp, under the 0.075 restore
        // point: the preset's own restore point (95 %) applies.
        let back = start + Duration::from_secs(4);
        let updates = run(&mut controller, back, 13, 8_000);
        assert!(updates.is_empty(), "no step up before 15 s");
        let updates = run(&mut controller, back + Duration::from_secs(14), 3, 8_000);
        assert_eq!(updates.len(), 1);
        let update = updates[0];
        assert_eq!(
            update.rung,
            Some(Rung {
                width: 1920,
                height: 1080,
                fps: 60
            })
        );
        assert_eq!(
            update.allocation_kbps,
            Some(8_640),
            "the step up raises the allocation"
        );
    }

    #[test]
    fn encoder_overload_steps_down_and_holds() {
        let (mut controller, start) = ultra(Instant::now());
        controller.set_encoder_load(EncoderLoad {
            frames_offered: 60,
            queue_drops: 6,
            mean_encode_ms: 12.0,
            fps: 60,
        });
        let updates = run(&mut controller, start, 3, 8_000);
        assert_eq!(updates.len(), 1);
        assert_eq!(controller.rung().fps, 30);
        // Still overloaded: it goes on down, 10 s apart, never up, whatever
        // the bitrate.
        let updates = run(&mut controller, start + Duration::from_secs(4), 30, 8_000);
        assert!(updates.iter().all(|u| u.rung.expect("rung").height < 1080));
        assert!(controller.rung().height <= 480);
    }

    #[test]
    fn the_top_rung_follows_libmello_encode_size_for() {
        assert_eq!(encode_size_for(1920, 1200, 1080, 60), (1728, 1080));
        assert_eq!(encode_size_for(3440, 1440, 1080, 60), (2576, 1080));
        assert_eq!(encode_size_for(1280, 720, 1080, 60), (1280, 720));
        assert_eq!(encode_size_for(1921, 1081, 1080, 60), (1920, 1080));
        let (w, h) = encode_size_for(3440, 1440, 1080, 120);
        assert!(h < 1080, "over level 5.1 at 120 fps");
        assert!(u64::from(w.div_ceil(16)) * u64::from(h.div_ceil(16)) * 120 <= 983_040);
    }

    #[test]
    fn overload_needs_a_full_window() {
        let busy = EncoderLoad {
            frames_offered: 10,
            queue_drops: 10,
            mean_encode_ms: 40.0,
            fps: 60,
        };
        assert!(!busy.overloaded());
        let slow = EncoderLoad {
            frames_offered: 60,
            queue_drops: 0,
            mean_encode_ms: 14.0,
            fps: 60,
        };
        assert!(slow.overloaded());
        let fine = EncoderLoad {
            frames_offered: 60,
            queue_drops: 1,
            mean_encode_ms: 6.0,
            fps: 60,
        };
        assert!(!fine.overloaded());
    }

    #[test]
    fn a_new_source_aspect_moves_the_geometry_of_the_current_rung() {
        let (mut controller, start) = ultra(Instant::now());
        run(&mut controller, start, 40, 600);
        assert_eq!(
            controller.rung(),
            Rung {
                width: 640,
                height: 360,
                fps: 30
            }
        );
        let rung = controller.set_top_size(2288, 960);
        assert_eq!(
            rung,
            Some(Rung {
                width: 864,
                height: 360,
                fps: 30
            })
        );
        assert_eq!(
            controller.set_top_size(2288, 960),
            None,
            "no change, no update"
        );
    }

    #[test]
    fn the_rung_message_round_trips_and_ignores_other_messages() {
        let rung = Rung {
            width: 1920,
            height: 1080,
            fps: 30,
        };
        let message = rung_message(rung);
        assert_eq!(message, [0x04, 0x05, 0x07, 0x80, 0x04, 0x38, 30]);
        assert_eq!(parse_rung_message(&message), Some(rung));
        assert_eq!(parse_rung_message(&[0x04, 0x03, 1]), None);
        assert_eq!(parse_rung_message(&message[..6]), None);
    }
}
