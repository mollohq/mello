use std::time::Instant;

use super::fec::FecDecoder;
use super::packet::{KeyframeRequest, LossReport, PacketType, StreamPacket};

/// Rate limit: at most one IDR request per 2 seconds.
const IDR_RATE_LIMIT_SECS: u64 = 2;

/// Number of consecutive unrecoverable FEC groups before requesting IDR.
const IDR_THRESHOLD: u32 = 2;

/// Viewer-side stream orchestration: FEC recovery, loss tracking, IDR requests.
pub struct StreamViewer {
    fec_decoder: FecDecoder,

    // Loss tracking (per 1-second window)
    packets_received: u16,
    packets_lost: u16,
    last_report_time: Instant,

    // IDR request state
    consecutive_unrecoverable: u32,
    last_idr_request: Instant,

    // FEC group tracking
    current_group_base: Option<u16>,
    group_packets_seen: usize,
}

/// Result of feeding a packet to the viewer.
pub enum ViewerFeedResult {
    /// A video payload is ready for decoding.
    VideoPayload { data: Vec<u8>, is_keyframe: bool },
    /// An audio payload is ready for playback.
    AudioPayload(Vec<u8>),
    /// A recovered video payload from FEC.
    RecoveredVideoPayload { data: Vec<u8>, is_keyframe: bool },
    /// A control action the caller should take.
    Action(ViewerAction),
    /// Packet was consumed but produced no output (e.g. FEC parity stored).
    None,
}

pub enum ViewerAction {
    /// Caller should send this control packet back to the host.
    SendControl(Vec<u8>),
}

impl StreamViewer {
    pub fn new(fec_n: usize) -> Self {
        let now = Instant::now();
        Self {
            fec_decoder: FecDecoder::new(fec_n),
            packets_received: 0,
            packets_lost: 0,
            last_report_time: now,
            consecutive_unrecoverable: 0,
            last_idr_request: now - std::time::Duration::from_secs(IDR_RATE_LIMIT_SECS + 1),
            current_group_base: None,
            group_packets_seen: 0,
        }
    }

    /// Feed a received wire-format packet. Returns what the caller should do.
    pub fn feed_packet(&mut self, data: &[u8]) -> Vec<ViewerFeedResult> {
        let mut results = Vec::new();

        let packet = match StreamPacket::parse(data) {
            Some(p) => p,
            _ => return results,
        };

        match packet.ptype {
            PacketType::Video => {
                self.packets_received += 1;
                self.on_video_packet(&packet, &mut results);
            }
            PacketType::Audio => {
                self.packets_received += 1;
                results.push(ViewerFeedResult::AudioPayload(packet.payload));
            }
            PacketType::Fec => {
                self.on_fec_packet(&packet, &mut results);
            }
            PacketType::Control => {
                // Control packets from host (future: quality change notifications)
            }
        }

        // Check if it's time to send a loss report (every 1 second)
        if let Some(action) = self.maybe_send_loss_report() {
            results.push(ViewerFeedResult::Action(action));
        }

        results
    }

    fn on_video_packet(&mut self, packet: &StreamPacket, results: &mut Vec<ViewerFeedResult>) {
        // Keyframe starts a fresh FEC group
        if packet.is_keyframe() {
            self.finalize_fec_group(results);
            self.current_group_base = Some(packet.sequence);
            self.group_packets_seen = 0;
            self.fec_decoder.reset(packet.sequence);
            self.consecutive_unrecoverable = 0;
        } else if self.current_group_base.is_none() {
            self.current_group_base = Some(packet.sequence);
            self.fec_decoder.reset(packet.sequence);
        }

        // Detect gaps: if sequence jumps, mark losses
        if let Some(base) = self.current_group_base {
            let expected_pos = self.group_packets_seen;
            let actual_pos = packet.sequence.wrapping_sub(base) as usize;
            if actual_pos > expected_pos {
                let gap = actual_pos - expected_pos;
                self.packets_lost += gap as u16;
            }
            self.group_packets_seen = actual_pos + 1;
        }

        let is_kf = packet.is_keyframe();

        // Feed to FEC decoder
        if let Some(recovered) = self.fec_decoder.feed_data(packet.sequence, &packet.payload) {
            results.push(ViewerFeedResult::RecoveredVideoPayload { data: recovered, is_keyframe: false });
        }

        results.push(ViewerFeedResult::VideoPayload { data: packet.payload.clone(), is_keyframe: is_kf });

        // If this is the last data packet in the FEC group, prepare for parity
        if packet.is_fec_group_last() {
            // Parity packet should arrive next
        }
    }

    fn on_fec_packet(&mut self, packet: &StreamPacket, results: &mut Vec<ViewerFeedResult>) {
        if let Some(recovered) = self.fec_decoder.feed_parity(&packet.payload) {
            self.packets_lost = self.packets_lost.saturating_sub(1);
            results.push(ViewerFeedResult::RecoveredVideoPayload { data: recovered, is_keyframe: false });
        }

        // FEC group is complete after parity — check if it was recoverable
        self.finalize_fec_group(results);

        // Reset for next group
        let next_base = self
            .current_group_base
            .map(|b| b.wrapping_add(self.fec_decoder.group_size() as u16));
        if let Some(base) = next_base {
            self.current_group_base = Some(base);
            self.group_packets_seen = 0;
            self.fec_decoder.reset(base);
        }
    }

    fn finalize_fec_group(&mut self, results: &mut Vec<ViewerFeedResult>) {
        if self.fec_decoder.is_unrecoverable() {
            self.consecutive_unrecoverable += 1;
            log::debug!(
                "Unrecoverable FEC group ({} consecutive)",
                self.consecutive_unrecoverable
            );

            if self.consecutive_unrecoverable >= IDR_THRESHOLD {
                if let Some(action) = self.maybe_request_idr() {
                    results.push(ViewerFeedResult::Action(action));
                }
            }
        } else {
            self.consecutive_unrecoverable = 0;
        }
    }

    fn maybe_request_idr(&mut self) -> Option<ViewerAction> {
        let now = Instant::now();
        if now.duration_since(self.last_idr_request).as_secs() < IDR_RATE_LIMIT_SECS {
            return None;
        }
        self.last_idr_request = now;
        self.consecutive_unrecoverable = 0;

        log::warn!("Requesting IDR from host (sustained packet loss)");
        let payload = KeyframeRequest::serialize();
        let packet = StreamPacket::control(payload, 0);
        Some(ViewerAction::SendControl(packet.serialize()))
    }

    fn maybe_send_loss_report(&mut self) -> Option<ViewerAction> {
        let now = Instant::now();
        if now.duration_since(self.last_report_time).as_secs() < 1 {
            return None;
        }

        let report = LossReport {
            packets_received: self.packets_received,
            packets_lost: self.packets_lost,
        };

        log::debug!(
            "Sending loss report: recv={} lost={} ({:.1}%)",
            report.packets_received,
            report.packets_lost,
            report.loss_ratio() * 100.0
        );

        // Reset counters for next window
        self.packets_received = 0;
        self.packets_lost = 0;
        self.last_report_time = now;

        let payload = report.serialize();
        let packet = StreamPacket::control(payload, 0);
        Some(ViewerAction::SendControl(packet.serialize()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_processes_video_packets() {
        let mut viewer = StreamViewer::new(3);
        let pkt = StreamPacket::video(vec![1, 2, 3], 0, true, false);
        let results = viewer.feed_packet(&pkt.serialize());
        let has_video = results.iter().any(|r| matches!(r, ViewerFeedResult::VideoPayload { is_keyframe: true, .. }));
        assert!(has_video);
    }

    #[test]
    fn viewer_processes_audio_packets() {
        let mut viewer = StreamViewer::new(3);
        let pkt = StreamPacket::audio(vec![0xAA; 160], 0);
        let results = viewer.feed_packet(&pkt.serialize());
        let has_audio = results.iter().any(|r| matches!(r, ViewerFeedResult::AudioPayload(_)));
        assert!(has_audio);
    }
}
