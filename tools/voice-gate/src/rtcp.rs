//! The send-leg feedback loop: the receiver reports that the SFU sends about
//! the sender's microphone track, and the sender reports they answer.
//!
//! Production (spec 10 sections 4.6 and 7.5). The SFU binds the client's
//! microphone track in Pion with `ConfigureRTCPReports`. Its receiver
//! interceptor (pion/interceptor `pkg/report`, v0.1.44) keeps per-stream
//! state for each RTP packet and sends one receiver report per second on a
//! ticker. The client's `rtc::RtcpSrReporter` sends a sender report with the
//! first packet and then with the first packet after each full second.
//! libmello reads each report block about the send SSRC
//! (`SendFeedbackTracker`) and gives it to the encoder adaptation with
//! `mello_voice_report_send_feedback`.
//!
//! Model. [`PionStream`] is a port of Pion's `receiverStream`: the same
//! sequence bitmap, cycle count, loss count since the last report, and
//! jitter. It sees each packet that the shim delivers, at its delivery time,
//! as the SFU sees it. [`RtcpModel`] adds the timing:
//!
//! - Receiver reports at receiver time 1000 k ms (the ticker starts with the
//!   track, before the first packet).
//! - Sender reports with the first sent packet and then with the first one
//!   after each 1000 ms. A sender report travels the RTP path: the shim's
//!   one-way delay at its send time, with its own jitter draw and with the
//!   stalls ([`Shim::one_way_delay_ms`]).
//! - A receiver report travels back to the sender on a path with the base
//!   delay and the jitter of the profile, without stalls (the stalls model
//!   the uplink). RTCP is never lost.
//! - The round trip is the sender report's forward delay plus the receiver
//!   report's reverse delay, as A - LSR - DLSR gives it.
//!
//! The jitter draws come from the profile seed's stream "rtcp", so the RTP
//! impairments of the frozen profiles do not change.
//!
//! [`FeedbackTracker`] is a port of libmello's `SendFeedbackTracker`: the
//! packets expected and lost since the previous report.

use crate::rng::Rng;
use crate::shim::Shim;
use std::collections::VecDeque;

/// Pion's receiver report interval.
pub const RR_INTERVAL_MS: i64 = 1000;
/// rtc::RtcpSrReporter sends at most one sender report per this interval.
pub const SR_INTERVAL_MS: i64 = 1000;
const CLOCK_RATE: f64 = 48_000.0;
/// Pion keeps 128 x 64 sequence numbers in its bitmap.
const HISTORY: usize = 128;
const HISTORY_BITS: u16 = (HISTORY * 64) as u16;
/// libmello SendFeedbackTracker: an interval longer than this is a reset.
const MAX_INTERVAL_PACKETS: u32 = 30_000;

/// The fields of one report block, as Pion fills them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReportBlock {
    pub fraction_lost: u8,
    pub cumulative_lost: i32,
    pub extended_highest_seq: u32,
    /// RTP timestamp units (48 kHz).
    pub jitter: u32,
}

/// Port of pion/interceptor `pkg/report/receiver_stream.go` (v0.1.44).
pub struct PionStream {
    packets: [u64; HISTORY],
    started: bool,
    seqnum_cycles: u16,
    last_seqnum: u16,
    last_report_seqnum: u16,
    last_rtp_time_rtp: u32,
    last_rtp_time_ms: f64,
    jitter: f64,
    total_lost: u32,
}

impl Default for PionStream {
    fn default() -> Self {
        Self {
            packets: [0; HISTORY],
            started: false,
            seqnum_cycles: 0,
            last_seqnum: 0,
            last_report_seqnum: 0,
            last_rtp_time_rtp: 0,
            last_rtp_time_ms: 0.0,
            jitter: 0.0,
            total_lost: 0,
        }
    }
}

impl PionStream {
    fn set_received(&mut self, seq: u16, received: bool) {
        let pos = seq % HISTORY_BITS;
        let bit = 1u64 << (pos % 64);
        let word = &mut self.packets[(pos / 64) as usize];
        if received {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }

    fn received(&self, seq: u16) -> bool {
        let pos = seq % HISTORY_BITS;
        self.packets[(pos / 64) as usize] & (1u64 << (pos % 64)) != 0
    }

    /// `processRTP`: one RTP packet arrived at `now_ms`.
    pub fn on_rtp(&mut self, now_ms: f64, seq: u16, timestamp: u32) {
        if !self.started {
            self.started = true;
            self.set_received(seq, true);
            self.last_seqnum = seq;
            self.last_report_seqnum = seq.wrapping_sub(1);
            self.last_rtp_time_rtp = timestamp;
            self.last_rtp_time_ms = now_ms;
            return;
        }
        self.set_received(seq, true);
        let diff = seq.wrapping_sub(self.last_seqnum);
        if diff > 0 && diff < (1 << 15) {
            if seq < self.last_seqnum {
                self.seqnum_cycles = self.seqnum_cycles.wrapping_add(1);
            }
            let mut i = self.last_seqnum.wrapping_add(1);
            while i != seq {
                self.set_received(i, false);
                i = i.wrapping_add(1);
            }
            self.last_seqnum = seq;
        }
        let d = (now_ms - self.last_rtp_time_ms) / 1000.0 * CLOCK_RATE
            - (f64::from(timestamp) - f64::from(self.last_rtp_time_rtp));
        self.jitter += (d.abs() - self.jitter) / 16.0;
        self.last_rtp_time_rtp = timestamp;
        self.last_rtp_time_ms = now_ms;
    }

    /// `generateReport`: the report block now.
    pub fn report(&mut self) -> ReportBlock {
        let total_since = self.last_seqnum.wrapping_sub(self.last_report_seqnum);
        let mut lost_since: u32 = 0;
        if self.last_seqnum != self.last_report_seqnum {
            let mut i = self.last_report_seqnum.wrapping_add(1);
            while i != self.last_seqnum {
                if !self.received(i) {
                    lost_since += 1;
                }
                i = i.wrapping_add(1);
            }
        }
        self.total_lost = (self.total_lost + lost_since).min(0xFF_FFFF);
        // Go: uint8(float64(lost*256) / float64(total)). 0 / 0 is NaN, and
        // NaN to uint8 gives 0 on amd64.
        let fraction = if total_since == 0 {
            0
        } else {
            (f64::from(lost_since.min(0xFF_FFFF) * 256) / f64::from(total_since)) as u8
        };
        let block = ReportBlock {
            fraction_lost: fraction,
            cumulative_lost: self.total_lost as i32,
            extended_highest_seq: (u32::from(self.seqnum_cycles) << 16)
                | u32::from(self.last_seqnum),
            jitter: self.jitter as u32,
        };
        self.last_report_seqnum = self.last_seqnum;
        block
    }
}

/// Port of libmello `SendFeedbackTracker::on_report` (voice_send_feedback.cpp).
pub struct FeedbackTracker {
    first_sequence: Option<u16>,
    previous: Option<(u32, i32)>,
    reports: u32,
}

impl FeedbackTracker {
    pub fn new(first_sequence: Option<u16>) -> Self {
        Self {
            first_sequence,
            previous: None,
            reports: 0,
        }
    }

    /// The feedback for one report block with round trip `rtt_ms` (< 0:
    /// unknown).
    pub fn on_report(&mut self, b: &ReportBlock, rtt_ms: f32) -> mello_sys::MelloSendFeedback {
        self.reports += 1;
        let before_first_packet = self.previous.is_none()
            && b.extended_highest_seq == 0
            && b.cumulative_lost == 0
            && b.fraction_lost == 0;
        let (expected, lost_delta) = if before_first_packet {
            (0u32, 0i64)
        } else if let Some((prev_seq, prev_lost)) = self.previous {
            let delta = b.extended_highest_seq.wrapping_sub(prev_seq);
            (
                if delta <= MAX_INTERVAL_PACKETS {
                    delta
                } else {
                    0
                },
                i64::from(b.cumulative_lost) - i64::from(prev_lost),
            )
        } else if let Some(first) = self.first_sequence {
            let delta = (b.extended_highest_seq as u16).wrapping_sub(first);
            (
                if delta < 0x8000 {
                    u32::from(delta) + 1
                } else {
                    0
                },
                i64::from(b.cumulative_lost),
            )
        } else {
            (0, 0)
        };
        let lost = if expected == 0 {
            0
        } else if lost_delta >= 0 && lost_delta <= i64::from(expected) {
            lost_delta as u32
        } else {
            (f64::from(b.fraction_lost) * f64::from(expected) / 256.0).round() as u32
        };
        if !before_first_packet {
            self.previous = Some((b.extended_highest_seq, b.cumulative_lost));
        }
        mello_sys::MelloSendFeedback {
            reports: self.reports,
            fraction_lost: f32::from(b.fraction_lost) / 256.0,
            cumulative_lost: b.cumulative_lost,
            extended_highest_seq: b.extended_highest_seq,
            jitter_ms: (f64::from(b.jitter) * 1000.0 / CLOCK_RATE) as f32,
            rtt_ms,
            interval_expected: expected,
            interval_lost: lost,
        }
    }
}

/// One receiver report on its way back to the sender.
#[derive(Clone, Copy, Debug)]
pub struct InFlightReport {
    /// Receiver time at which the sender gets it.
    pub arrive_ms: i64,
    /// Receiver time at which the SFU sent it.
    pub sent_ms: i64,
    pub feedback: mello_sys::MelloSendFeedback,
}

/// The SFU's receiver reports and the sender's sender reports. See the
/// module notes.
pub struct RtcpModel {
    stream: PionStream,
    tracker: Option<FeedbackTracker>,
    rng: Rng,
    last_sr_sent_ms: Option<i64>,
    /// Sender reports in flight: (arrival, send time).
    sr_in_flight: VecDeque<(i64, i64)>,
    /// The last sender report that reached the SFU: (send time, arrival).
    last_sr: Option<(i64, i64)>,
    next_rr_ms: i64,
    rr_in_flight: VecDeque<InFlightReport>,
    pub reports_sent: u32,
}

impl RtcpModel {
    pub fn new(seed: u64) -> Self {
        Self {
            stream: PionStream::default(),
            tracker: None,
            rng: Rng::stream(seed, "rtcp"),
            last_sr_sent_ms: None,
            sr_in_flight: VecDeque::new(),
            last_sr: None,
            next_rr_ms: RR_INTERVAL_MS,
            rr_in_flight: VecDeque::new(),
            reports_sent: 0,
        }
    }

    /// The sender sent RTP sequence `rtp_seq` at `now_ms`: the first packet
    /// fixes the tracker's start (libmello: the packetizer's first sequence),
    /// and a sender report goes with it when one is due.
    pub fn on_rtp_sent(&mut self, now_ms: i64, rtp_seq: u16, shim: &Shim) {
        if self.tracker.is_none() {
            self.tracker = Some(FeedbackTracker::new(Some(rtp_seq)));
        }
        let due = self
            .last_sr_sent_ms
            .is_none_or(|last| now_ms >= last + SR_INTERVAL_MS);
        if due {
            self.last_sr_sent_ms = Some(now_ms);
            let delay = shim.one_way_delay_ms(now_ms as f64, self.rng.gaussian());
            let arrive = now_ms + delay.ceil() as i64;
            // A path keeps order: a sender report never overtakes the one
            // before it.
            let arrive = self
                .sr_in_flight
                .back()
                .map_or(arrive, |&(a, _)| arrive.max(a));
            self.sr_in_flight.push_back((arrive, now_ms));
        }
    }

    /// The SFU received RTP sequence `seq` with timestamp `timestamp` at
    /// `now_ms` (the shim delivered it).
    pub fn on_rtp_received(&mut self, now_ms: i64, seq: u16, timestamp: u32) {
        self.stream.on_rtp(now_ms as f64, seq, timestamp);
    }

    /// Advance to `now_ms`: sender reports arrive, and a receiver report
    /// leaves when the ticker fires.
    pub fn step(&mut self, now_ms: i64, shim: &Shim) {
        while let Some(&(arrive, sent)) = self.sr_in_flight.front() {
            if arrive > now_ms {
                break;
            }
            self.last_sr = Some((sent, arrive));
            self.sr_in_flight.pop_front();
        }
        if now_ms < self.next_rr_ms {
            return;
        }
        self.next_rr_ms += RR_INTERVAL_MS;
        let block = self.stream.report();
        let (base, sigma) = shim.base_and_sigma_ms(now_ms as f64);
        let reverse = (base + self.rng.gaussian() * sigma).max(0.0);
        // A - LSR - DLSR: the sender report's forward delay plus this
        // report's reverse delay.
        let rtt_ms = self.last_sr.map_or(-1.0, |(sent, arrive)| {
            ((arrive - sent) as f64 + reverse) as f32
        });
        let feedback = match self.tracker.as_mut() {
            Some(t) => t.on_report(&block, rtt_ms),
            // Before the first packet: Pion's empty report.
            None => FeedbackTracker::new(None).on_report(&block, rtt_ms),
        };
        self.reports_sent += 1;
        let arrive_ms = now_ms + reverse.ceil() as i64;
        let arrive_ms = self
            .rr_in_flight
            .back()
            .map_or(arrive_ms, |r| arrive_ms.max(r.arrive_ms));
        self.rr_in_flight.push_back(InFlightReport {
            arrive_ms,
            sent_ms: now_ms,
            feedback,
        });
    }

    /// Receiver reports that reach the sender at or before `now_ms`.
    pub fn arrived(&mut self, now_ms: i64) -> Vec<InFlightReport> {
        let mut out = Vec::new();
        while let Some(r) = self.rr_in_flight.front() {
            if r.arrive_ms > now_ms {
                break;
            }
            if let Some(r) = self.rr_in_flight.pop_front() {
                out.push(r);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pion_stream_counts_loss_since_the_last_report() {
        let mut s = PionStream::default();
        // Before the first packet: Pion's empty report.
        assert_eq!(s.report(), ReportBlock::default());
        // 65530..=65535, 0..=9 with 3 and 4 missing: 16 sequence numbers.
        let mut seq: u16 = 65530;
        for i in 0..16u32 {
            if seq != 3 && seq != 4 {
                s.on_rtp(f64::from(i) * 20.0, seq, i * 960);
            }
            seq = seq.wrapping_add(1);
        }
        let b = s.report();
        assert_eq!(b.cumulative_lost, 2);
        assert_eq!(b.fraction_lost, (2 * 256 / 16) as u8);
        assert_eq!(b.extended_highest_seq, (1 << 16) | 9);
        assert_eq!(b.jitter, 0, "packets on time give no jitter");
        // A late packet that arrives before the next report is not lost.
        s.on_rtp(400.0, 12, 14 * 960);
        s.on_rtp(410.0, 11, 13 * 960);
        s.on_rtp(420.0, 10, 12 * 960);
        let b = s.report();
        assert_eq!(b.cumulative_lost, 2);
        assert_eq!(b.fraction_lost, 0);
        assert!(b.jitter > 0);
        // No packet since the last report.
        let b2 = s.report();
        assert_eq!(b2.fraction_lost, 0);
        assert_eq!(b2.extended_highest_seq, b.extended_highest_seq);
    }

    #[test]
    fn tracker_matches_libmello() {
        // The same trace as libmello's
        // VoiceSendFeedback.TrackerGivesPacketsExpectedAndLostPerReport.
        let mut t = FeedbackTracker::new(Some(65530));
        let fb = t.on_report(&ReportBlock::default(), -1.0);
        assert_eq!(fb.interval_expected, 0);
        let mut b = ReportBlock {
            fraction_lost: 20,
            cumulative_lost: 2,
            extended_highest_seq: (1 << 16) | 19,
            jitter: 96,
        };
        let fb = t.on_report(&b, -1.0);
        assert_eq!((fb.interval_expected, fb.interval_lost), (26, 2));
        assert!((fb.jitter_ms - 2.0).abs() < 1e-6);
        b.extended_highest_seq += 50;
        b.cumulative_lost = 7;
        let fb = t.on_report(&b, 62.5);
        assert_eq!((fb.interval_expected, fb.interval_lost), (50, 5));
        b.extended_highest_seq += 40;
        b.cumulative_lost = 1;
        b.fraction_lost = 64;
        let fb = t.on_report(&b, 62.5);
        assert_eq!((fb.interval_expected, fb.interval_lost), (40, 10));
        let fb = t.on_report(&b, 62.5);
        assert_eq!((fb.interval_expected, fb.interval_lost), (0, 0));
        b.extended_highest_seq -= 100;
        let fb = t.on_report(&b, 62.5);
        assert_eq!((fb.interval_expected, fb.interval_lost), (0, 0));
        assert_eq!(fb.reports, 6);
    }
}
