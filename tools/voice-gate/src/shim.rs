//! Impairment shim: what the network and the SFU do to a packet between
//! `mello_voice_get_packet` on the sender and `mello_voice_feed_packet` on
//! the receiver.
//!
//! Header rewrite. On the SFU path the sender's 4-byte little-endian
//! sequence is stripped (mello-core, voice/mod.rs) and the packet travels as
//! RTP with a 16-bit sequence. The receiver rebuilds a 4-byte header from the
//! RTP sequence: low two bytes = the 16-bit sequence, high two bytes = 0
//! (libmello/src/transport/peer_connection.cpp, incoming audio track). The
//! shim does the same: RTP sequence = (start + sender sequence) mod 65536.

use crate::profile::{Network, Profile};
use crate::rng::Rng;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Counters for one run.
#[derive(Clone, Debug, Default)]
pub struct ShimCounters {
    pub sent: u64,
    pub lost_random: u64,
    pub lost_burst: u64,
    pub lost_outage: u64,
    pub reordered: u64,
    pub delivered: u64,
}

impl ShimCounters {
    pub fn lost(&self) -> u64 {
        self.lost_random + self.lost_burst + self.lost_outage
    }
}

struct InFlight {
    deliver_ms: i64,
    order: u64,
    bytes: Vec<u8>,
}

impl PartialEq for InFlight {
    fn eq(&self, other: &Self) -> bool {
        (self.deliver_ms, self.order) == (other.deliver_ms, other.order)
    }
}
impl Eq for InFlight {}
impl PartialOrd for InFlight {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for InFlight {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.deliver_ms, self.order).cmp(&(other.deliver_ms, other.order))
    }
}

pub struct Shim {
    net: Network,
    rtp_seq_start: u16,
    rng_loss: Rng,
    rng_burst: Rng,
    rng_delay: Rng,
    rng_reorder: Rng,
    gilbert_bad: bool,
    /// Stall windows in ms of run time: random spikes and episode stalls.
    stalls: Vec<(f64, f64)>,
    last_fifo_ms: f64,
    in_flight: BinaryHeap<Reverse<InFlight>>,
    order: u64,
    pub counters: ShimCounters,
    /// Sender sample position when the first packet with a wrapped RTP
    /// sequence was sent (None when the run does not wrap).
    pub wrap_sender_sample: Option<usize>,
}

impl Shim {
    pub fn new(profile: &Profile) -> Self {
        let net = profile.network.clone();
        let mut stalls = Vec::new();
        if net.spikes_per_min > 0.0 && net.spike_ms > 0.0 {
            let mut r = Rng::stream(profile.seed, "spikes");
            let mean_gap_ms = 60_000.0 / net.spikes_per_min;
            let end = profile.duration_s * 1000.0;
            let mut t = r.exponential(mean_gap_ms);
            while t < end {
                stalls.push((t, t + net.spike_ms));
                t += net.spike_ms + r.exponential(mean_gap_ms);
            }
        }
        for e in &net.episodes {
            if e.stall_ms > 0.0 {
                let s = e.start_s * 1000.0;
                stalls.push((s, s + e.stall_ms));
            }
        }
        Self {
            rtp_seq_start: profile.rtp_seq_start,
            rng_loss: Rng::stream(profile.seed, "loss"),
            rng_burst: Rng::stream(profile.seed, "burst"),
            rng_delay: Rng::stream(profile.seed, "delay"),
            rng_reorder: Rng::stream(profile.seed, "reorder"),
            gilbert_bad: false,
            stalls,
            last_fifo_ms: 0.0,
            in_flight: BinaryHeap::new(),
            order: 0,
            counters: ShimCounters::default(),
            wrap_sender_sample: None,
            net,
        }
    }

    /// Conditions at run time `t_ms`: (jitter sigma, random loss).
    fn conditions(&self, t_ms: f64) -> (f64, f64) {
        for e in &self.net.episodes {
            let s = e.start_s * 1000.0;
            if t_ms >= s && t_ms < s + e.dur_s * 1000.0 {
                return (e.jitter_sigma_ms, e.loss_random);
            }
        }
        (self.net.jitter_sigma_ms, self.net.loss_random)
    }

    /// Take one packet from `mello_voice_get_packet` sent at run time
    /// `now_ms`. `sender_sample` is the sender capture position at that time.
    pub fn send(&mut self, packet: &[u8], now_ms: i64, sender_sample: usize) {
        if packet.len() < 4 {
            return;
        }
        self.counters.sent += 1;
        let sender_seq = u32::from_le_bytes([packet[0], packet[1], packet[2], packet[3]]);
        let wide = u32::from(self.rtp_seq_start) + sender_seq;
        if wide >= 65_536 && self.wrap_sender_sample.is_none() {
            self.wrap_sender_sample = Some(sender_sample);
        }
        let rtp_seq = (wide & 0xFFFF) as u16;
        let mut bytes = packet.to_vec();
        bytes[0] = rtp_seq as u8;
        bytes[1] = (rtp_seq >> 8) as u8;
        bytes[2] = 0;
        bytes[3] = 0;

        let t = now_ms as f64;
        let (sigma, loss_random) = self.conditions(t);

        // Every random stream advances once per packet, lost or not, so the
        // pattern of one impairment does not depend on another.
        let u_loss = self.rng_loss.uniform();
        let burst_lost = match &self.net.burst {
            Some(g) => {
                let u_state = self.rng_burst.uniform();
                let u_drop = self.rng_burst.uniform();
                self.gilbert_bad = if self.gilbert_bad {
                    u_state >= g.p_exit
                } else {
                    u_state < g.p_enter
                };
                self.gilbert_bad && u_drop < g.loss_in_bad
            }
            None => false,
        };
        let jitter = self.rng_delay.gaussian() * sigma;
        let u_reorder = self.rng_reorder.uniform();

        let in_outage = self
            .net
            .outage
            .map(|(s, d)| t >= s * 1000.0 && t < (s + d) * 1000.0)
            .unwrap_or(false);
        if in_outage {
            self.counters.lost_outage += 1;
            return;
        }
        if burst_lost {
            self.counters.lost_burst += 1;
            return;
        }
        if u_loss < loss_random {
            self.counters.lost_random += 1;
            return;
        }

        let mut deliver = t + (self.net.base_delay_ms + jitter).max(0.0);
        for &(s, e) in &self.stalls {
            if t >= s && t < e {
                deliver = deliver.max(e + self.net.base_delay_ms);
            }
        }
        if u_reorder < self.net.reorder_prob {
            // Arrives late and out of order; does not hold back the queue.
            deliver += self.net.reorder_extra_ms;
            self.counters.reordered += 1;
        } else {
            // A network path keeps order: a packet never overtakes the one
            // before it.
            deliver = deliver.max(self.last_fifo_ms);
            self.last_fifo_ms = deliver;
        }
        self.order += 1;
        self.in_flight.push(Reverse(InFlight {
            deliver_ms: deliver.ceil() as i64,
            order: self.order,
            bytes,
        }));
    }

    /// Packets due at or before `now_ms`, in delivery order.
    pub fn due(&mut self, now_ms: i64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(Reverse(top)) = self.in_flight.peek() {
            if top.deliver_ms > now_ms {
                break;
            }
            if let Some(Reverse(p)) = self.in_flight.pop() {
                out.push(p.bytes);
            }
        }
        self.counters.delivered += out.len() as u64;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Gilbert, Network, Profile};

    fn profile(net: Network, seq: u16) -> Profile {
        Profile {
            name: "t".into(),
            description: String::new(),
            duration_s: 10.0,
            seed: 9,
            rtp_seq_start: seq,
            drift_ppm: 0.0,
            sender: Default::default(),
            network: net,
            gates: Vec::new(),
        }
    }

    fn pkt(seq: u32) -> Vec<u8> {
        let mut v = seq.to_le_bytes().to_vec();
        v.extend_from_slice(&[0xAA; 20]);
        v
    }

    #[test]
    fn rewrites_header_like_the_sfu_path_and_wraps() {
        let net = Network {
            base_delay_ms: 20.0,
            ..Default::default()
        };
        let mut shim = Shim::new(&profile(net, 65534));
        for s in 0..4u32 {
            shim.send(&pkt(s), i64::from(s) * 20, s as usize * 960);
        }
        let got = shim.due(1000);
        let seqs: Vec<(u8, u8, u8, u8)> = got.iter().map(|p| (p[0], p[1], p[2], p[3])).collect();
        assert_eq!(
            seqs,
            vec![
                (0xFE, 0xFF, 0, 0),
                (0xFF, 0xFF, 0, 0),
                (0, 0, 0, 0),
                (1, 0, 0, 0)
            ]
        );
        assert_eq!(shim.wrap_sender_sample, Some(2 * 960));
    }

    #[test]
    fn fifo_order_holds_without_reorder() {
        let net = Network {
            base_delay_ms: 30.0,
            jitter_sigma_ms: 40.0,
            ..Default::default()
        };
        let mut shim = Shim::new(&profile(net, 0));
        for s in 0..200u32 {
            shim.send(&pkt(s), i64::from(s) * 20, 0);
        }
        let got = shim.due(1_000_000);
        let seqs: Vec<u16> = got
            .iter()
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted);
    }

    #[test]
    fn loss_rates_follow_the_profile() {
        let net = Network {
            base_delay_ms: 10.0,
            loss_random: 0.05,
            burst: Some(Gilbert {
                p_enter: 0.01,
                p_exit: 0.25,
                loss_in_bad: 1.0,
            }),
            ..Default::default()
        };
        let mut shim = Shim::new(&profile(net, 0));
        let n = 50_000u32;
        for s in 0..n {
            shim.send(&pkt(s), i64::from(s) * 20, 0);
        }
        let random = shim.counters.lost_random as f64 / f64::from(n);
        let burst = shim.counters.lost_burst as f64 / f64::from(n);
        // Stationary bad-state share: p_enter / (p_enter + p_exit) = 3.8 %.
        assert!((burst - 0.0385).abs() < 0.008, "burst {burst}");
        // Random loss applies to packets the burst did not take.
        assert!(
            (random - 0.05 * (1.0 - burst)).abs() < 0.006,
            "random {random}"
        );
    }

    #[test]
    fn outage_drops_everything_in_the_window() {
        let net = Network {
            base_delay_ms: 10.0,
            outage: Some((1.0, 2.0)),
            ..Default::default()
        };
        let mut shim = Shim::new(&profile(net, 0));
        for s in 0..250u32 {
            shim.send(&pkt(s), i64::from(s) * 20, 0);
        }
        assert_eq!(shim.counters.lost_outage, 100);
    }
}
