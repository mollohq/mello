//! The game clip upload's budget while the user streams (12-STREAMING.md
//! 3.7, G2 and G4). The stream has priority. The upload gets bandwidth only
//! while the stream holds its top rung with no trouble for 15 s, and it
//! pauses at once on a rung drop, an overload or a goog_cc target under the
//! stream's bitrate: the upload sheds before the stream moves.
//!
//! goog_cc's target is capped at the stream's preset (`sender_limits_kbps`),
//! so it cannot show headroom over a stream at its preset. That is G4: a
//! fixed 500 kbps. A target under the preset gives G2's share of the gap
//! between the target and the stream's bitrate, which is small: the bitrate
//! follows the target.

use std::time::{Duration, Instant};

/// G2: the most the upload takes during a stream.
pub const MAX_BPS: u64 = 1_000_000;
/// G2: the upload's share of the headroom.
pub const HEADROOM_PERCENT: u64 = 25;
/// G4: the rate when goog_cc shows nothing over the stream.
pub const FIXED_BPS: u64 = 500_000;
/// G3: the time at the top rung before the upload goes on, the controller's
/// own rung-up hold.
pub const RESUME_HOLD: Duration = Duration::from_secs(15);
/// A target at this share of the preset is at the sender's ceiling.
const PINNED_PERCENT: u64 = 95;

/// The stream as the quality controller sees it.
#[derive(Debug, Clone, Copy)]
pub struct StreamLoad {
    pub top_rung: bool,
    pub overloaded: bool,
    /// goog_cc's encoder share in kbps, `None` before the first feedback.
    pub gcc_kbps: Option<u32>,
    /// The encoder bitrate in kbps.
    pub bitrate_kbps: u32,
    pub preset_kbps: u32,
}

/// See the module documentation.
#[derive(Debug, Default)]
pub struct UploadGovernor {
    /// Since when the stream has had no trouble at its top rung.
    healthy_since: Option<Instant>,
}

impl UploadGovernor {
    /// The upload budget in bps now. 0 pauses the upload.
    pub fn budget_bps(&mut self, load: StreamLoad, now: Instant) -> u64 {
        let under_stream = load.gcc_kbps.is_some_and(|gcc| gcc < load.bitrate_kbps);
        if !load.top_rung || load.overloaded || under_stream {
            self.healthy_since = None;
            return 0;
        }
        let since = *self.healthy_since.get_or_insert(now);
        if now.duration_since(since) < RESUME_HOLD {
            return 0;
        }
        match load.gcc_kbps {
            None => FIXED_BPS,
            Some(gcc) if u64::from(gcc) * 100 >= u64::from(load.preset_kbps) * PINNED_PERCENT => {
                FIXED_BPS
            }
            Some(gcc) => {
                let headroom_bps = u64::from(gcc.saturating_sub(load.bitrate_kbps)) * 1000;
                (headroom_bps * HEADROOM_PERCENT / 100).min(MAX_BPS)
            }
        }
    }
}

/// The stream's hold on the upload pace. The upload pauses from the start
/// of the stream until the governor gives a budget. Dropping the hold gives
/// the pace back to the game rule, so every way out of a stream ends it.
#[derive(Debug)]
pub struct UploadPaceHold(());

impl UploadPaceHold {
    pub fn new() -> Self {
        crate::clip_upload::pace().set_stream_budget(0);
        Self(())
    }

    pub fn set(&self, bps: u64) {
        crate::clip_upload::pace().set_stream_budget(bps);
    }
}

impl Default for UploadPaceHold {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for UploadPaceHold {
    fn drop(&mut self) {
        crate::clip_upload::pace().clear_stream();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_preset() -> StreamLoad {
        StreamLoad {
            top_rung: true,
            overloaded: false,
            gcc_kbps: Some(6_000),
            bitrate_kbps: 6_000,
            preset_kbps: 6_000,
        }
    }

    #[test]
    fn the_upload_waits_15_s_at_the_top_rung() {
        let mut g = UploadGovernor::default();
        let t0 = Instant::now();
        assert_eq!(g.budget_bps(at_preset(), t0), 0);
        assert_eq!(g.budget_bps(at_preset(), t0 + Duration::from_secs(14)), 0);
        assert_eq!(
            g.budget_bps(at_preset(), t0 + RESUME_HOLD),
            FIXED_BPS,
            "G4: the target is at the ceiling"
        );
    }

    #[test]
    fn any_trouble_pauses_at_once_and_restarts_the_hold() {
        let t0 = Instant::now();
        let healthy = t0 + RESUME_HOLD;
        for trouble in [
            StreamLoad {
                top_rung: false,
                ..at_preset()
            },
            StreamLoad {
                overloaded: true,
                ..at_preset()
            },
            StreamLoad {
                gcc_kbps: Some(5_000),
                ..at_preset()
            },
        ] {
            let mut g = UploadGovernor::default();
            g.budget_bps(at_preset(), t0);
            assert_eq!(g.budget_bps(at_preset(), healthy), FIXED_BPS);
            let t1 = healthy + Duration::from_secs(1);
            assert_eq!(g.budget_bps(trouble, t1), 0, "{trouble:?}");
            // The hold starts again at the first tick with no trouble.
            let t2 = t1 + Duration::from_secs(1);
            assert_eq!(g.budget_bps(at_preset(), t2), 0);
            assert_eq!(g.budget_bps(at_preset(), t2 + Duration::from_secs(14)), 0);
            assert_eq!(g.budget_bps(at_preset(), t2 + RESUME_HOLD), FIXED_BPS);
        }
    }

    #[test]
    fn no_estimate_gives_the_fixed_rate() {
        let mut g = UploadGovernor::default();
        let load = StreamLoad {
            gcc_kbps: None,
            ..at_preset()
        };
        let t0 = Instant::now();
        g.budget_bps(load, t0);
        assert_eq!(g.budget_bps(load, t0 + RESUME_HOLD), FIXED_BPS);
    }

    #[test]
    fn a_target_under_the_preset_gives_a_quarter_of_the_headroom() {
        let mut g = UploadGovernor::default();
        let load = StreamLoad {
            gcc_kbps: Some(4_400),
            bitrate_kbps: 4_000,
            ..at_preset()
        };
        let t0 = Instant::now();
        g.budget_bps(load, t0);
        assert_eq!(g.budget_bps(load, t0 + RESUME_HOLD), 100_000);
        // The share never goes over 1 Mbps.
        let wide = StreamLoad {
            gcc_kbps: Some(5_500),
            bitrate_kbps: 300,
            ..at_preset()
        };
        assert_eq!(g.budget_bps(wide, t0 + RESUME_HOLD), MAX_BPS);
    }
}
