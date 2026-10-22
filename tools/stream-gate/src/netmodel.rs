//! The impairment model of one direction of one leg: the voice gate's shim
//! (tools/voice-gate/src/shim.rs, profile.rs) moved to packets, with a
//! bandwidth cap and a tail-drop queue added.
//!
//! The model is a pure function of the packets it is given and their times:
//! the UDP proxy (`shim.rs`) feeds it every datagram of a direction and
//! sends the datagram at the time the model returns, or drops it.
//!
//! Random draws (shim version [`SHIM_VERSION`] = 1).
//!
//! | Draw | Keyed by | Stream |
//! |---|---|---|
//! | random loss | packet ordinal in this direction | `loss` |
//! | Gilbert-Elliott state | 10 ms time slot since the arm time | `burst` (state step, then drop draw) |
//! | burst drop | packet ordinal | `burst-drop` |
//! | jitter | 10 ms time slot | `delay` (one standard normal per slot) |
//! | reorder | packet ordinal | `reorder` |
//! | delay spikes | time (exponential gaps) | `spikes` |
//!
//! Loss is per packet: a video frame of 20 packets meets 20 independent
//! draws. Jitter and the burst state are per time slot, so the packets of
//! one slot share one delay, as on a real queue. Each stream has its own
//! generator (`Rng::stream`) seeded from the profile seed and the leg and
//! direction name, so one impairment never moves the draws of another.
//!
//! Before the arm time the model only adds the base delay: ICE, DTLS and the
//! first offer and answer meet a clean path.

use voice_gate::rng::Rng;

/// The rule for random draws; `profiles.json` carries it as `"shim"`.
pub const SHIM_VERSION: u64 = 1;
/// Width of one time slot for jitter and the burst state.
pub const SLOT_US: u64 = 10_000;

/// Gilbert-Elliott burst loss, stepped once per time slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Gilbert {
    pub p_enter: f64,
    pub p_exit: f64,
    pub loss_in_bad: f64,
}

/// A bandwidth cap with a tail-drop queue in a time window.
#[derive(Clone, Debug, PartialEq)]
pub struct Cap {
    pub start_s: f64,
    pub dur_s: f64,
    /// The cap as a fraction of the lane's preset bitrate.
    pub rate_fraction: f64,
    /// Filled in from the lane: `rate_fraction` x preset.
    pub rate_kbps: f64,
    /// A packet that finds more than this backlog is dropped.
    pub queue_ms: f64,
    /// Bytes per datagram on the link besides the UDP payload: IPv4 and UDP
    /// headers.
    pub overhead_bytes: f64,
}

/// Impairments of one leg. The forward direction (host to SFU on the host
/// leg, SFU to viewer on a viewer leg) gets all of them. The reverse
/// direction gets the base delay and the outage only.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Leg {
    pub base_delay_ms: f64,
    pub jitter_sigma_ms: f64,
    pub loss_random: f64,
    pub burst: Option<Gilbert>,
    pub spikes_per_min: f64,
    pub spike_ms: f64,
    pub reorder_prob: f64,
    pub reorder_extra_ms: f64,
    /// Every packet in [start, start + dur), both directions, is lost.
    pub outage: Option<(f64, f64)>,
    pub cap: Option<Cap>,
    /// One-way delay grows by this many parts per million of the time since
    /// the arm time: the receive clock and the send clock drift apart.
    pub drift_ppm: f64,
}

/// What happened to a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// Deliver at this time (µs since the model's time origin).
    Deliver(u64),
    LostRandom,
    LostBurst,
    LostOutage,
    LostQueue,
}

#[derive(Clone, Debug, Default)]
pub struct Counters {
    pub packets: u64,
    pub bytes: u64,
    pub lost_random: u64,
    pub lost_burst: u64,
    pub lost_outage: u64,
    pub lost_queue: u64,
    pub reordered: u64,
}

impl Counters {
    pub fn lost(&self) -> u64 {
        self.lost_random + self.lost_burst + self.lost_outage + self.lost_queue
    }
}

/// One direction of one leg.
pub struct Direction {
    leg: Leg,
    forward: bool,
    rng_loss: Rng,
    rng_burst: Rng,
    rng_burst_drop: Rng,
    rng_delay: Rng,
    rng_reorder: Rng,
    /// Burst state and jitter draw per slot, from slot 0 up.
    slots: Vec<(bool, f64)>,
    gilbert_bad: bool,
    stalls: Vec<(u64, u64)>,
    last_fifo_us: u64,
    link_free_us: f64,
    pub counters: Counters,
}

impl Direction {
    /// `name` keys the random streams ("host/fwd", "viewer1/rev", ...).
    /// `duration_s` bounds the spike schedule.
    pub fn new(leg: &Leg, forward: bool, seed: u64, name: &str, duration_s: f64) -> Self {
        let s = |what: &str| Rng::stream(seed, &format!("{name}/{what}"));
        let mut stalls = Vec::new();
        if forward && leg.spikes_per_min > 0.0 && leg.spike_ms > 0.0 {
            let mut r = s("spikes");
            let mean_gap_us = 60e6 / leg.spikes_per_min;
            let end = duration_s * 1e6;
            let mut t = r.exponential(mean_gap_us);
            while t < end {
                stalls.push((t as u64, (t + leg.spike_ms * 1000.0) as u64));
                t += leg.spike_ms * 1000.0 + r.exponential(mean_gap_us);
            }
        }
        Self {
            leg: leg.clone(),
            forward,
            rng_loss: s("loss"),
            rng_burst: s("burst"),
            rng_burst_drop: s("burst-drop"),
            rng_delay: s("delay"),
            rng_reorder: s("reorder"),
            slots: Vec::new(),
            gilbert_bad: false,
            stalls,
            last_fifo_us: 0,
            link_free_us: 0.0,
            counters: Counters::default(),
        }
    }

    pub fn stalls(&self) -> &[(u64, u64)] {
        &self.stalls
    }

    fn slot(&mut self, t_us: u64) -> (bool, f64) {
        let slot = (t_us / SLOT_US) as usize;
        while self.slots.len() <= slot {
            let bad = match &self.leg.burst {
                Some(g) => {
                    let u = self.rng_burst.uniform();
                    self.gilbert_bad = if self.gilbert_bad {
                        u >= g.p_exit
                    } else {
                        u < g.p_enter
                    };
                    self.gilbert_bad
                }
                None => false,
            };
            let g = self.rng_delay.gaussian();
            self.slots.push((bad, g));
        }
        self.slots[slot]
    }

    /// The fate of one datagram of `bytes` bytes that the sender hands to the
    /// network at `now_us`. `armed_us` is the arm time on the same clock, or
    /// None before the arm. Times are µs on the caller's clock.
    pub fn send(&mut self, now_us: u64, armed_us: Option<u64>, bytes: usize) -> Fate {
        self.counters.packets += 1;
        self.counters.bytes += bytes as u64;
        let base_us = (self.leg.base_delay_ms * 1000.0) as u64;
        let Some(arm) = armed_us else {
            return self.fifo(now_us + base_us);
        };
        let t = now_us.saturating_sub(arm);
        let ts = t as f64 / 1e6;

        if let Some((s, d)) = self.leg.outage {
            if ts >= s && ts < s + d {
                self.counters.lost_outage += 1;
                return Fate::LostOutage;
            }
        }
        if !self.forward {
            return self.fifo(now_us + base_us);
        }

        // Draws in a fixed order for every packet, lost or not, so one
        // impairment never shifts the draws of another.
        let u_loss = self.rng_loss.uniform();
        let u_drop = self.rng_burst_drop.uniform();
        let u_reorder = self.rng_reorder.uniform();
        let (bad, gauss) = self.slot(t);

        if let Some(g) = &self.leg.burst {
            if bad && u_drop < g.loss_in_bad {
                self.counters.lost_burst += 1;
                return Fate::LostBurst;
            }
        }
        if u_loss < self.leg.loss_random {
            self.counters.lost_random += 1;
            return Fate::LostRandom;
        }

        // Bandwidth cap: the link serializes at the rate; a packet that finds
        // more than queue_ms of backlog is dropped.
        let mut departs = now_us as f64;
        if let Some(c) = &self.leg.cap {
            let (start, end) = (c.start_s * 1e6, (c.start_s + c.dur_s) * 1e6);
            let tf = t as f64;
            if tf >= start && tf < end && c.rate_kbps > 0.0 {
                let backlog_us = (self.link_free_us - now_us as f64).max(0.0);
                if backlog_us > c.queue_ms * 1000.0 {
                    self.counters.lost_queue += 1;
                    return Fate::LostQueue;
                }
                let ser_us = (bytes as f64 + c.overhead_bytes) * 8.0 * 1000.0 / c.rate_kbps;
                self.link_free_us = self.link_free_us.max(now_us as f64) + ser_us;
                departs = self.link_free_us;
            }
        }

        let jitter_us = gauss * self.leg.jitter_sigma_ms * 1000.0;
        let drift_us = self.leg.drift_ppm * 1e-6 * t as f64;
        let mut deliver = departs + (base_us as f64 + jitter_us).max(0.0) + drift_us;
        for &(s, e) in &self.stalls {
            if t >= s && t < e {
                deliver = deliver.max((arm + e + base_us) as f64);
            }
        }
        let deliver = deliver.ceil() as u64;
        if u_reorder < self.leg.reorder_prob {
            // Late and out of order; it does not hold back the queue.
            self.counters.reordered += 1;
            return Fate::Deliver(deliver + (self.leg.reorder_extra_ms * 1000.0) as u64);
        }
        self.fifo(deliver)
    }

    /// A network path keeps order: a packet never overtakes the one before.
    fn fifo(&mut self, deliver_us: u64) -> Fate {
        let d = deliver_us.max(self.last_fifo_us);
        self.last_fifo_us = d;
        Fate::Deliver(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg() -> Leg {
        Leg {
            base_delay_ms: 10.0,
            ..Default::default()
        }
    }

    /// 1000 packets per second for `secs`, armed at 0. Returns the fates.
    fn pump(d: &mut Direction, secs: u64, bytes: usize) -> Vec<Fate> {
        (0..secs * 1000)
            .map(|i| d.send(1_000_000 + i * 1000, Some(1_000_000), bytes))
            .collect()
    }

    #[test]
    fn same_seed_same_fates() {
        let mut l = leg();
        l.loss_random = 0.05;
        l.jitter_sigma_ms = 20.0;
        l.burst = Some(Gilbert {
            p_enter: 0.01,
            p_exit: 0.3,
            loss_in_bad: 0.8,
        });
        let a = pump(&mut Direction::new(&l, true, 7, "v/fwd", 10.0), 10, 1200);
        let b = pump(&mut Direction::new(&l, true, 7, "v/fwd", 10.0), 10, 1200);
        assert_eq!(a, b);
        let c = pump(&mut Direction::new(&l, true, 8, "v/fwd", 10.0), 10, 1200);
        assert_ne!(a, c);
    }

    #[test]
    fn random_loss_follows_the_profile() {
        let mut l = leg();
        l.loss_random = 0.05;
        let mut d = Direction::new(&l, true, 1, "x", 60.0);
        pump(&mut d, 60, 1000);
        let rate = d.counters.lost_random as f64 / d.counters.packets as f64;
        assert!((rate - 0.05).abs() < 0.005, "{rate}");
    }

    #[test]
    fn burst_loss_comes_in_runs() {
        let mut l = leg();
        l.burst = Some(Gilbert {
            p_enter: 0.02,
            p_exit: 0.25,
            loss_in_bad: 1.0,
        });
        let mut d = Direction::new(&l, true, 3, "x", 60.0);
        let fates = pump(&mut d, 60, 1000);
        // Stationary bad share: 0.02 / 0.27 = 7.4 %.
        let rate = d.counters.lost_burst as f64 / d.counters.packets as f64;
        assert!((rate - 0.074).abs() < 0.02, "{rate}");
        // Runs: a 10 ms bad slot loses its 10 packets in a row.
        let mut longest = 0;
        let mut run = 0;
        for f in &fates {
            if *f == Fate::LostBurst {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
        assert!(longest >= 10, "{longest}");
    }

    #[test]
    fn order_holds_without_reorder() {
        let mut l = leg();
        l.jitter_sigma_ms = 30.0;
        let mut d = Direction::new(&l, true, 5, "x", 10.0);
        let times: Vec<u64> = pump(&mut d, 10, 500)
            .into_iter()
            .map(|f| match f {
                Fate::Deliver(t) => t,
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn the_reverse_direction_gets_base_delay_and_outage_only() {
        let mut l = leg();
        l.loss_random = 0.5;
        l.outage = Some((1.0, 2.0));
        let mut d = Direction::new(&l, false, 5, "x", 10.0);
        let fates = pump(&mut d, 4, 100);
        assert_eq!(d.counters.lost_random, 0);
        assert_eq!(d.counters.lost_outage, 2000);
        assert_eq!(fates[0], Fate::Deliver(1_010_000));
    }

    #[test]
    fn nothing_is_lost_before_the_arm() {
        let mut l = leg();
        l.loss_random = 1.0;
        let mut d = Direction::new(&l, true, 5, "x", 10.0);
        assert_eq!(d.send(500, None, 100), Fate::Deliver(10_500));
        assert_eq!(d.send(600, Some(600), 100), Fate::LostRandom);
    }

    #[test]
    fn the_cap_queues_then_drops_the_excess() {
        // 8 Mbps offered (1000 B per ms) into a 4 Mbps cap with a 100 ms
        // queue, for 2 s from t = 1 s.
        let mut l = leg();
        l.cap = Some(Cap {
            start_s: 1.0,
            dur_s: 2.0,
            rate_fraction: 0.5,
            rate_kbps: 4000.0,
            queue_ms: 100.0,
            overhead_bytes: 0.0,
        });
        let mut d = Direction::new(&l, true, 5, "x", 4.0);
        let fates = pump(&mut d, 4, 1000);
        let in_window = &fates[1000..3000];
        let dropped = in_window.iter().filter(|f| **f == Fate::LostQueue).count();
        // Half the packets fit the link; the queue absorbs about 100 more.
        assert!((850..=1000).contains(&dropped), "{dropped}");
        // The delay at the window end is the base plus about the queue.
        let (i, t) = (1000..3000)
            .rev()
            .find_map(|i| match fates[i] {
                Fate::Deliver(t) => Some((i as u64, t)),
                _ => None,
            })
            .expect("packets pass in the window");
        let delay_ms = (t - (1_000_000 + i * 1000)) as f64 / 1000.0;
        assert!((95.0..=115.0).contains(&delay_ms), "{delay_ms}");
        // After the window: base delay again, once the backlog is gone.
        assert_eq!(fates[3500], Fate::Deliver(1_000_000 + 3500 * 1000 + 10_000));
    }

    #[test]
    fn an_outage_drops_everything_in_its_window() {
        let mut l = leg();
        l.outage = Some((1.0, 3.0));
        let mut d = Direction::new(&l, true, 5, "x", 5.0);
        pump(&mut d, 5, 100);
        assert_eq!(d.counters.lost_outage, 3000);
        assert_eq!(d.counters.lost(), 3000);
    }

    #[test]
    fn drift_grows_the_delay() {
        let mut l = leg();
        l.drift_ppm = 1000.0;
        let mut d = Direction::new(&l, true, 5, "x", 20.0);
        let f = d.send(1_000_000 + 10_000_000, Some(1_000_000), 100);
        // 10 s at 1000 ppm: 10 ms more than the base delay.
        assert_eq!(f, Fate::Deliver(1_000_000 + 10_000_000 + 20_000));
    }
}
