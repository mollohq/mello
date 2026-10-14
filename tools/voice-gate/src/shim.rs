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
//!
//! Random draws per send slot (shim version [`SHIM_VERSION`] = 2). The
//! network is a function of time, not of the packet count. Each random
//! stream (loss, burst state and drop, jitter, reorder) gives one set of
//! draws per 20 ms send slot. The slot of a packet is its media time: the
//! sender's RTP timestamp offset (`SentPacket::timestamp`, 48 kHz samples
//! from the first captured sample of the run) divided by 960, the samples of
//! one 20 ms frame. To reach the slot of a packet, each stream advances
//! through every slot up to and including that slot. A slot without a
//! packet (a DTX gap or a closed gate of N frames) consumes its draws all the
//! same, and the Gilbert-Elliott burst state takes one step per slot. So a
//! sender that sends one packet more or less meets the same network in every
//! later slot. The streams stay independent: each has its own generator
//! (`Rng::stream`), and a draw of one never moves another. Two packets with
//! the same slot get the same draws. Shim version 1 (baseline 2.9) advanced
//! every stream once per packet instead.

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
    /// Lost packets (not DTX) of a talk spurt of which no earlier packet
    /// was delivered: NetEQ has played nothing of the spurt yet, so it has
    /// nothing to conceal (see [`SpurtLosses`]).
    pub lost_spurt_head: u64,
    spurts: SpurtLosses,
}

impl ShimCounters {
    pub fn lost(&self) -> u64 {
        self.lost_random + self.lost_burst + self.lost_outage + self.lost_queue
    }
}

/// Talk spurts of the sender's packet stream, for the concealment ratio.
///
/// A loss is concealable only when NetEQ has played at least one frame of
/// the loss's talk spurt before it. A lost first packet of a spurt (the
/// pre-roll frame that opens it, or more) is not: NetEQ plays comfort noise
/// or is muted, and starts the spurt with the next packet that arrives.
/// Those losses leave the denominator of `conceal_per_lost`, as lost DTX
/// frames do.
///
/// A spurt starts at the first packet, and at a packet whose timestamp moved
/// more than `GAP_FRAMES` frames further than its sequence: the sender's
/// gate was closed (the rule of the receiver's talk-spurt controller, spec
/// 10 section 5.2). Packets come in send order.
#[derive(Clone, Debug, Default)]
pub struct SpurtLosses {
    prev: Option<(u32, u32)>,
    delivered_in_spurt: bool,
}

impl SpurtLosses {
    const FRAME: i64 = 960;
    const GAP_FRAMES: i64 = 2;

    /// Record one sent packet. Returns true when it is lost and no earlier
    /// packet of its spurt was delivered.
    pub fn on_packet(&mut self, sequence: u32, timestamp: u32, lost: bool) -> bool {
        let new_spurt = match self.prev {
            None => true,
            Some((seq, ts)) => {
                let seq_step = i64::from(sequence.wrapping_sub(seq) as i32);
                let ts_step = i64::from(timestamp.wrapping_sub(ts) as i32);
                ts_step > (seq_step + Self::GAP_FRAMES) * Self::FRAME
            }
        };
        self.prev = Some((sequence, timestamp));
        if new_spurt {
            self.delivered_in_spurt = false;
        }
        if lost {
            !self.delivered_in_spurt
        } else {
            self.delivered_in_spurt = true;
            false
        }
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

/// The shim's rule for random draws. `profiles.json` and
/// `profiles-congestion.json` carry it as `"shim"`, so a change of the rule
/// changes `inputs_fingerprint`. 1: one draw per packet (baseline 2.9).
/// 2: one draw per 20 ms send slot (see the module notes).
pub const SHIM_VERSION: u64 = 2;

/// Samples of one 20 ms frame at 48 kHz: the width of one send slot in RTP
/// timestamp units.
pub const SLOT_SAMPLES: u32 = 960;

/// The send slot of a packet: its media time in 20 ms frames.
pub fn slot_of(timestamp: u32) -> usize {
    (timestamp / SLOT_SAMPLES) as usize
}

/// The random draws of one 20 ms send slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotDraws {
    /// Uniform for the random loss.
    pub u_loss: f64,
    /// Gilbert-Elliott state after this slot's step (false without burst
    /// loss).
    pub burst_bad: bool,
    /// Uniform for the drop in the bad state.
    pub u_burst_drop: f64,
    /// Standard normal for the jitter, scaled by the sigma at send time.
    pub gaussian: f64,
    /// Uniform for the reorder.
    pub u_reorder: f64,
}

/// The random streams of the shim, drawn slot by slot.
pub struct SlotDrawer {
    burst: Option<crate::profile::Gilbert>,
    rng_loss: Rng,
    rng_burst: Rng,
    rng_delay: Rng,
    rng_reorder: Rng,
    gilbert_bad: bool,
    /// Draws of every slot from 0 up to the highest slot asked for.
    slots: Vec<SlotDraws>,
}

impl SlotDrawer {
    pub fn new(seed: u64, net: &Network) -> Self {
        Self {
            burst: net.burst.clone(),
            rng_loss: Rng::stream(seed, "loss"),
            rng_burst: Rng::stream(seed, "burst"),
            rng_delay: Rng::stream(seed, "delay"),
            rng_reorder: Rng::stream(seed, "reorder"),
            gilbert_bad: false,
            slots: Vec::new(),
        }
    }

    /// The draws of `slot`. Every stream advances through every slot up to
    /// and including `slot`, also through slots without a packet.
    pub fn draws(&mut self, slot: usize) -> SlotDraws {
        while self.slots.len() <= slot {
            let u_loss = self.rng_loss.uniform();
            let (burst_bad, u_burst_drop) = match &self.burst {
                Some(g) => {
                    let u_state = self.rng_burst.uniform();
                    let u_drop = self.rng_burst.uniform();
                    self.gilbert_bad = if self.gilbert_bad {
                        u_state >= g.p_exit
                    } else {
                        u_state < g.p_enter
                    };
                    (self.gilbert_bad, u_drop)
                }
                None => (false, 1.0),
            };
            let gaussian = self.rng_delay.gaussian();
            let u_reorder = self.rng_reorder.uniform();
            self.slots.push(SlotDraws {
                u_loss,
                burst_bad,
                u_burst_drop,
                gaussian,
                u_reorder,
            });
        }
        self.slots[slot]
    }
}

pub struct Shim {
    net: Network,
    rtp_seq_start: u16,
    rtp_ts_start: u32,
    ssrc: u32,
    drawer: SlotDrawer,
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
    /// Slot and draws of every sent packet, for the tests.
    #[cfg(test)]
    used: Vec<(usize, SlotDraws)>,
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
            drawer: SlotDrawer::new(profile.seed, &net),
            stalls,
            last_fifo_ms: 0.0,
            link_free_ms: 0.0,
            in_flight: BinaryHeap::new(),
            order: 0,
            counters: ShimCounters::default(),
            wrap_sender_sample: None,
            #[cfg(test)]
            used: Vec::new(),
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

    /// Whether the bottleneck queue is full at run time `t_ms`: the test of
    /// [`Self::through_bottleneck`], without changing the link.
    fn would_drop_in_bottleneck(&self, t_ms: f64) -> bool {
        let Some(b) = &self.net.bottleneck else {
            return false;
        };
        let (start, end) = (b.start_s * 1000.0, (b.start_s + b.dur_s) * 1000.0);
        t_ms >= start && t_ms < end && (self.link_free_ms - t_ms).max(0.0) > b.queue_ms
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

        // The draws of the packet's send slot (module notes): the same for
        // this slot whatever the sender sent before it.
        let slot = slot_of(packet.timestamp);
        let d = self.drawer.draws(slot);
        #[cfg(test)]
        self.used.push((slot, d));
        let u_loss = d.u_loss;
        let burst_lost = match &self.net.burst {
            Some(g) => d.burst_bad && d.u_burst_drop < g.loss_in_bad,
            None => false,
        };
        let jitter = d.gaussian * sigma;
        let u_reorder = d.u_reorder;

        let in_outage = self
            .net
            .outage
            .map(|(s, d)| t >= s * 1000.0 && t < (s + d) * 1000.0)
            .unwrap_or(false);
        let dtx = u64::from(packet.payload.len() <= 2);
        let lost = in_outage || burst_lost || u_loss < loss_random;
        self.counters.sent_log.push((now_ms, lost));
        // The bottleneck queue decides further down; the tracker needs the
        // final answer now.
        let queue_lost = !lost && self.would_drop_in_bottleneck(t);
        if self
            .counters
            .spurts
            .on_packet(packet.sequence, packet.timestamp, lost || queue_lost)
            && dtx == 0
        {
            self.counters.lost_spurt_head += 1;
        }
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
            group: "t".into(),
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

    /// A mobile-like network: every random stream in use.
    fn noisy() -> Network {
        Network {
            base_delay_ms: 50.0,
            jitter_sigma_ms: 15.0,
            loss_random: 0.03,
            burst: Some(Gilbert {
                p_enter: 0.008,
                p_exit: 0.35,
                loss_in_bad: 1.0,
            }),
            reorder_prob: 0.005,
            reorder_extra_ms: 40.0,
            ..Default::default()
        }
    }

    /// Send one packet per slot in `slots`, at the slot's send time, with
    /// the sender's sequence counting the sent packets.
    fn send_slots(net: Network, slots: &[u32]) -> Shim {
        let mut shim = Shim::new(&profile(net, 0));
        for (seq, &slot) in slots.iter().enumerate() {
            let p = SentPacket {
                chunks: 0,
                sequence: seq as u32,
                timestamp: slot * SLOT_SAMPLES,
                payload: vec![0xAA; 20],
            };
            shim.send(&p, i64::from(slot) * 20, 0);
        }
        shim
    }

    #[test]
    fn one_packet_less_meets_the_same_network() {
        // Sender A sends every slot of 0..400. Sender B is the same sender
        // with the packet of slot 100 not sent: its later packets have a
        // sequence one lower. Shim version 1 shifted every later draw by one
        // packet; now every later slot gets the same draws and the same
        // loss decision.
        let all: Vec<u32> = (0..400).collect();
        let fewer: Vec<u32> = all.iter().copied().filter(|&s| s != 100).collect();
        let a = send_slots(noisy(), &all);
        let b = send_slots(noisy(), &fewer);
        assert_eq!(b.used.len() + 1, a.used.len());
        let a_by_slot: std::collections::BTreeMap<usize, (SlotDraws, bool)> = a
            .used
            .iter()
            .zip(&a.counters.sent_log)
            .map(|(&(slot, d), &(_, lost))| (slot, (d, lost)))
            .collect();
        let mut later = 0;
        for (&(slot, d), &(_, lost)) in b.used.iter().zip(&b.counters.sent_log) {
            assert_eq!(a_by_slot[&slot], (d, lost), "slot {slot}");
            later += usize::from(slot > 100);
        }
        assert_eq!(later, 299);
        // The network has losses, so the test can see a shift.
        assert!(a.counters.lost() > 10, "lost {}", a.counters.lost());
    }

    #[test]
    fn a_slot_has_the_same_draws_after_skipped_slots() {
        let net = noisy();
        let mut every = SlotDrawer::new(303, &net);
        let reference: Vec<SlotDraws> = (0..=300).map(|s| every.draws(s)).collect();
        // A sender with a gate gap: slots 0..20, then nothing up to 120, a
        // DTX-like gap of single frames, then a long gap to 300.
        let mut gappy = SlotDrawer::new(303, &net);
        let asked: Vec<usize> = (0..20)
            .chain(120..140)
            .chain((140..200).step_by(3))
            .chain([300])
            .collect();
        for &s in &asked {
            assert_eq!(gappy.draws(s), reference[s], "slot {s}");
        }
        // A slot asked again, or out of order, keeps its draws.
        assert_eq!(gappy.draws(130), reference[130]);
        assert_eq!(gappy.draws(50), reference[50]);
        // The first packet late in the run: every earlier slot still draws.
        let mut late = SlotDrawer::new(303, &net);
        assert_eq!(late.draws(250), reference[250]);
        // The burst state is part of the draws: the chain stepped in the
        // skipped slots, so the bad periods are the same.
        assert!(reference.iter().any(|d| d.burst_bad));
    }

    #[test]
    fn a_sender_without_gaps_keeps_the_per_packet_draws() {
        // One packet in every slot (push-to-talk, the drift profile): the
        // draws are the ones of shim version 1, one per packet.
        let net = noisy();
        let mut slots = SlotDrawer::new(606, &net);
        let mut loss = Rng::stream(606, "loss");
        let mut burst = Rng::stream(606, "burst");
        let mut delay = Rng::stream(606, "delay");
        let mut reorder = Rng::stream(606, "reorder");
        for s in 0..500 {
            let d = slots.draws(s);
            assert_eq!(d.u_loss, loss.uniform());
            let _state = burst.uniform();
            assert_eq!(d.u_burst_drop, burst.uniform());
            assert_eq!(d.gaussian, delay.gaussian());
            assert_eq!(d.u_reorder, reorder.uniform());
        }
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
    fn a_lost_spurt_head_is_not_concealable() {
        let mut t = SpurtLosses::default();
        // Spurt 1: frames 0..5, frame 2 lost after frames 0 and 1 played.
        let s1: Vec<bool> = (0..5u32).map(|i| t.on_packet(i, i * 960, i == 2)).collect();
        assert_eq!(s1, [false; 5]);
        // Spurt 2 after a 1 s gate gap: its first two packets lost (the
        // pre-roll), then delivered, then one more loss inside it.
        let base = 5 * 960 + 48_000;
        let s2: Vec<bool> = (0..6u32)
            .map(|i| t.on_packet(5 + i, base + i * 960, i < 2 || i == 4))
            .collect();
        assert_eq!(s2, [true, true, false, false, false, false]);
        // A lost packet (sequence and timestamp move together) or a 2-frame
        // jump is not a new spurt.
        assert!(!t.on_packet(12, base + 7 * 960, true));
        assert!(!t.on_packet(13, base + 10 * 960, true));
        // 3 frames further than the sequence: the gate was closed.
        assert!(t.on_packet(14, base + 14 * 960, true));
    }

    #[test]
    fn the_shim_counts_lost_spurt_heads() {
        // Spurt 1 (5 packets from 0 ms), spurt 2 after a 1 s gate gap (5
        // packets from 1000 ms). The outage takes the first two of spurt 2.
        let mut p = profile(
            Network {
                base_delay_ms: 10.0,
                outage: Some((1.0, 0.04)),
                ..Default::default()
            },
            0,
        );
        p.duration_s = 10.0;
        let mut shim = Shim::new(&p);
        for s in 0..5u32 {
            shim.send(&pkt(s), i64::from(s) * 20, 0);
        }
        for i in 0..5u32 {
            let q = SentPacket {
                timestamp: 5 * 960 + 48_000 + i * 960,
                ..pkt(5 + i)
            };
            shim.send(&q, 1000 + i64::from(i) * 20, 0);
        }
        assert_eq!(shim.counters.lost(), 2);
        assert_eq!(shim.counters.lost_spurt_head, 2);
        // The same outage inside a spurt: concealable.
        let mut q = profile(
            Network {
                base_delay_ms: 10.0,
                outage: Some((0.04, 0.02)),
                ..Default::default()
            },
            0,
        );
        q.duration_s = 10.0;
        let mut inside = Shim::new(&q);
        for s in 0..5u32 {
            inside.send(&pkt(s), i64::from(s) * 20, 0);
        }
        assert_eq!(inside.counters.lost(), 1);
        assert_eq!(inside.counters.lost_spurt_head, 0);
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
