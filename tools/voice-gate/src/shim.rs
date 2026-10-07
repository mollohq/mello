//! Impairment shim: what the network and the SFU do to a packet between the
//! sender's packet sink and `mello_voice_feed_rtp` on the receiver.
//!
//! RTP. On the SFU path the sender's packetizer sends the Opus payload as
//! RTP: payload type 111, a 16-bit sequence, a timestamp that is a random
//! start plus the media time, and an SSRC. The SFU forwards it with the SSRC
//! of the receiver's track (libmello/src/transport/peer_connection.cpp and
//! mello-sfu). The receiver gives the whole packet to NetEQ. The shim builds
//! the same packet: the sequence is (rtp_seq_start + sender sequence) mod
//! 65536, the timestamp is the start plus the media time mod 2^32, and the
//! SSRC is fixed. The timestamp start and the SSRC come from the profile seed
//! (stream "rtp"), so the frozen profile inputs do not change.

use crate::profile::{Network, Profile};
use crate::rng::Rng;
use crate::run::SentPacket;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Counters for one run.
#[derive(Clone, Debug, Default)]
pub struct ShimCounters {
    pub sent: u64,
    pub lost_random: u64,
    pub lost_burst: u64,
    pub lost_outage: u64,
    /// Dropped by a full bottleneck queue.
    pub lost_queue: u64,
    pub reordered: u64,
    pub delivered: u64,
    /// Lost packets that carried an Opus DTX frame (2 bytes or less): the
    /// receiver plays comfort noise through them, nothing to conceal.
    pub lost_dtx: u64,
    /// Every sent packet: run time of the send, and whether the network
    /// lost it. The truth that the receiver reports estimate.
    pub sent_log: Vec<(i64, bool)>,
}

impl ShimCounters {
    pub fn lost(&self) -> u64 {
        self.lost_random + self.lost_burst + self.lost_outage + self.lost_queue
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

/// Payload type of Opus on the SFU path.
const OPUS_PAYLOAD_TYPE: u8 = 111;

/// A complete RTP packet (version 2, no CSRC, no extension).
pub fn rtp_packet(sequence: u16, timestamp: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + payload.len());
    p.push(0x80);
    p.push(OPUS_PAYLOAD_TYPE);
    p.extend_from_slice(&sequence.to_be_bytes());
    p.extend_from_slice(&timestamp.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

pub struct Shim {
    net: Network,
    rtp_seq_start: u16,
    rtp_ts_start: u32,
    ssrc: u32,
    rng_loss: Rng,
    rng_burst: Rng,
    rng_delay: Rng,
    rng_reorder: Rng,
    gilbert_bad: bool,
    /// Stall windows in ms of run time: random spikes and episode stalls.
    stalls: Vec<(f64, f64)>,
    last_fifo_ms: f64,
    /// Bottleneck: run time at which the link has sent its backlog.
    link_free_ms: f64,
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
        let mut rtp_rng = Rng::stream(profile.seed, "rtp");
        let rtp_ts_start = (rtp_rng.next_u64() >> 32) as u32;
        let ssrc = (rtp_rng.next_u64() >> 32) as u32;
        Self {
            rtp_seq_start: profile.rtp_seq_start,
            rtp_ts_start,
            ssrc,
            rng_loss: Rng::stream(profile.seed, "loss"),
            rng_burst: Rng::stream(profile.seed, "burst"),
            rng_delay: Rng::stream(profile.seed, "delay"),
            rng_reorder: Rng::stream(profile.seed, "reorder"),
            gilbert_bad: false,
            stalls,
            last_fifo_ms: 0.0,
            link_free_ms: 0.0,
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

    /// Pass `bytes` (an RTP or RTCP packet) through the bottleneck at run
    /// time `t_ms`. Returns the time it leaves the link (`t_ms` outside the
    /// window), or None when the queue is full and drops it.
    fn through_bottleneck(&mut self, t_ms: f64, bytes: usize) -> Option<f64> {
        let Some(b) = &self.net.bottleneck else {
            return Some(t_ms);
        };
        let (start, end) = (b.start_s * 1000.0, (b.start_s + b.dur_s) * 1000.0);
        if t_ms < start || t_ms >= end {
            return Some(t_ms);
        }
        let backlog = (self.link_free_ms - t_ms).max(0.0);
        if backlog > b.queue_ms {
            return None;
        }
        let ser_ms = (bytes as f64 + b.overhead_bytes) * 8.0 / b.rate_kbps;
        self.link_free_ms = self.link_free_ms.max(t_ms) + ser_ms;
        Some(self.link_free_ms)
    }

    /// Take one packet from the sender's packet sink, sent at run time
    /// `now_ms`. `sender_sample` is the sender capture position at that time.
    pub fn send(&mut self, packet: &SentPacket, now_ms: i64, sender_sample: usize) {
        if packet.payload.is_empty() {
            return;
        }
        self.counters.sent += 1;
        let wide = u32::from(self.rtp_seq_start) + packet.sequence;
        if wide >= 65_536 && self.wrap_sender_sample.is_none() {
            self.wrap_sender_sample = Some(sender_sample);
        }
        let rtp_seq = (wide & 0xFFFF) as u16;
        let bytes = rtp_packet(
            rtp_seq,
            self.rtp_ts_start.wrapping_add(packet.timestamp),
            self.ssrc,
            &packet.payload,
        );

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
        let dtx = u64::from(packet.payload.len() <= 2);
        let lost = in_outage || burst_lost || u_loss < loss_random;
        self.counters.sent_log.push((now_ms, lost));
        if in_outage {
            self.counters.lost_outage += 1;
            self.counters.lost_dtx += dtx;
            return;
        }
        if burst_lost {
            self.counters.lost_burst += 1;
            self.counters.lost_dtx += dtx;
            return;
        }
        if u_loss < loss_random {
            self.counters.lost_random += 1;
            self.counters.lost_dtx += dtx;
            return;
        }

        let Some(departs) = self.through_bottleneck(t, bytes.len()) else {
            self.counters.lost_queue += 1;
            self.counters.lost_dtx += dtx;
            if let Some(last) = self.counters.sent_log.last_mut() {
                last.1 = true;
            }
            return;
        };
        let mut deliver = departs + (self.net.base_delay_ms + jitter).max(0.0);
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

    /// One-way delay in ms of a packet sent at run time `t_ms` with a
    /// standard normal jitter draw `gaussian`: the base delay, the jitter of
    /// the conditions at `t_ms`, and a stall that holds the packet to its
    /// end. The RTCP model (rtcp.rs) uses it for the sender reports, which
    /// travel the same path as the RTP packets. FIFO order and reorder do
    /// not apply.
    /// A sender report is `bytes` long on the link and queues behind the
    /// RTP packets at a bottleneck; None when the queue drops it.
    pub fn one_way_delay_ms(&mut self, t_ms: f64, gaussian: f64, bytes: usize) -> Option<f64> {
        let (sigma, _) = self.conditions(t_ms);
        let departs = self.through_bottleneck(t_ms, bytes)?;
        let mut deliver = departs + (self.net.base_delay_ms + gaussian * sigma).max(0.0);
        for &(s, e) in &self.stalls {
            if t_ms >= s && t_ms < e {
                deliver = deliver.max(e + self.net.base_delay_ms);
            }
        }
        Some(deliver - t_ms)
    }

    /// Base delay and jitter of the network at run time `t_ms`, for a path
    /// without stalls (the reverse path of the receiver reports).
    pub fn base_and_sigma_ms(&self, t_ms: f64) -> (f64, f64) {
        (self.net.base_delay_ms, self.conditions(t_ms).0)
    }

    /// The RTP sequence number that the shim gives the sender's packet
    /// `sequence`.
    pub fn rtp_sequence(&self, sequence: u32) -> u16 {
        ((u32::from(self.rtp_seq_start) + sequence) & 0xFFFF) as u16
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
            adapt: true,
            added: false,
        }
    }

    fn pkt(seq: u32) -> SentPacket {
        SentPacket {
            chunks: 0,
            sequence: seq,
            timestamp: seq * 960,
            payload: vec![0xAA; 20],
        }
    }

    fn rtp_seq(p: &[u8]) -> u16 {
        u16::from_be_bytes([p[2], p[3]])
    }

    fn rtp_ts(p: &[u8]) -> u32 {
        u32::from_be_bytes([p[4], p[5], p[6], p[7]])
    }

    #[test]
    fn builds_rtp_like_the_sfu_path_and_wraps() {
        let net = Network {
            base_delay_ms: 20.0,
            ..Default::default()
        };
        let mut shim = Shim::new(&profile(net, 65534));
        for s in 0..4u32 {
            shim.send(&pkt(s), i64::from(s) * 20, s as usize * 960);
        }
        let got = shim.due(1000);
        let seqs: Vec<u16> = got.iter().map(|p| rtp_seq(p)).collect();
        assert_eq!(seqs, vec![65534, 65535, 0, 1]);
        assert_eq!(shim.wrap_sender_sample, Some(2 * 960));
        for (i, p) in got.iter().enumerate() {
            assert_eq!(p[0], 0x80, "RTP version 2, no padding, extension or CSRC");
            assert_eq!(p[1], 111, "Opus payload type, no marker");
            assert_eq!(&p[12..], &[0xAA; 20], "the Opus payload follows the header");
            let step = rtp_ts(p).wrapping_sub(rtp_ts(&got[0]));
            assert_eq!(
                step,
                i as u32 * 960,
                "the timestamp advances with media time"
            );
            assert_eq!(&p[8..12], &got[0][8..12], "one SSRC per stream");
        }
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
        let seqs: Vec<u16> = got.iter().map(|p| rtp_seq(p)).collect();
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
    fn bottleneck_queues_then_drops_the_excess_rate() {
        use crate::profile::Bottleneck;
        // 84 kbps of packets (198 + 12 bytes per 20 ms) into a 56 kbps link
        // with a 300 ms queue, from 1 s to 3 s.
        let net = Network {
            base_delay_ms: 10.0,
            bottleneck: Some(Bottleneck {
                start_s: 1.0,
                dur_s: 2.0,
                rate_kbps: 56.0,
                queue_ms: 300.0,
                overhead_bytes: 12.0,
            }),
            ..Default::default()
        };
        let mut shim = Shim::new(&profile(net, 0));
        let mut sent_at = Vec::new();
        for s in 0..250u32 {
            let p = SentPacket {
                payload: vec![0xAA; 198],
                ..pkt(s)
            };
            shim.send(&p, i64::from(s) * 20, 0);
            sent_at.push(i64::from(s) * 20);
        }
        let lost = shim.counters.lost_queue;
        // 100 packets in the window, the link takes 2/3 of them, less the
        // queue it fills.
        assert!((25..=40).contains(&lost), "queue drops {lost}");
        assert_eq!(shim.counters.lost(), lost);
        assert!(shim.counters.sent_log.iter().filter(|x| x.1).count() as u64 == lost);
        // Before the window: 10 ms. At its end: the queue, at most 300 ms
        // plus one packet on the link.
        let got = shim.due(10_000);
        assert_eq!(got.len() as u64, 250 - lost);
        let mut fresh = Shim::new(&profile(
            Network {
                base_delay_ms: 10.0,
                ..Default::default()
            },
            0,
        ));
        assert_eq!(fresh.one_way_delay_ms(500.0, 0.0, 80), Some(10.0));
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
