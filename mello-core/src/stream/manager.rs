use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio::time::MissedTickBehavior;

use super::abr::AbrController;
use super::config::StreamConfig;
use super::fec::FecEncoder;
use super::input::{InputPassthrough, InputPassthroughStub};
use super::pacer::{calc_stream_pacing_target_kbps, PacingTelemetry};
use super::packet::{ControlSubtype, LossReport, PacketType, StreamPacket};
use super::sink::PacketSink;

const AUDIO_PACING_BUDGET_KBPS: u32 = 160;
const PACING_TELEMETRY_INTERVAL_SECS: u64 = 2;
const MANAGER_TELEMETRY_INTERVAL_SECS: u64 = 1;
const MAX_VIDEO_COALESCE_DRAIN: usize = 32;
const QUEUE_KEYFRAME_COALESCE_THRESHOLD: usize = 2;
const QUEUE_RECOVERY_COALESCE_THRESHOLD: usize = 10;
const QUEUE_KEYFRAME_REQUEST_COOLDOWN_SECS: u64 = 2;

pub struct VideoPacket {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub timestamp: u64,
}

pub struct AudioPacket {
    pub data: Vec<u8>,
    pub timestamp: u64,
}

/// Active streaming session returned by `start_stream`.
pub struct StreamSession {
    pub session_id: String,
    pub mode: String,
    stop_tx: Option<oneshot::Sender<()>>,
}

impl StreamSession {
    pub fn new(session_id: String, mode: String, stop_tx: oneshot::Sender<()>) -> Self {
        Self {
            session_id,
            mode,
            stop_tx: Some(stop_tx),
        }
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The stream manager orchestrates the host-side streaming pipeline:
/// receives encoded packets from libmello via channels, applies FEC, and sends through the sink.
pub struct StreamManager {
    #[allow(dead_code)]
    ctx: *mut mello_sys::MelloContext,
    host: *mut mello_sys::MelloStreamHost,
    sink: Arc<dyn PacketSink>,
    fec_encoder: FecEncoder,
    video_seq: AtomicU16,
    audio_seq: AtomicU16,
    abr: AbrController,
    config: StreamConfig,
    #[allow(dead_code)]
    input: Arc<dyn InputPassthrough>,
    video_rx: mpsc::Receiver<VideoPacket>,
    audio_rx: mpsc::Receiver<AudioPacket>,
    last_queue_keyframe_request: Instant,
    last_pacing_telemetry: Option<PacingTelemetry>,
    last_pacing_sample_at: Instant,
    manager_video_packets_in_total: u64,
    manager_audio_packets_in_total: u64,
    manager_video_packets_coalesced_total: u64,
    manager_video_coalesce_events_total: u64,
    manager_keyframe_req_queue_pressure_total: u64,
    manager_keyframe_req_recovery_total: u64,
    manager_keyframe_req_viewer_total: u64,
    manager_video_dropped_for_recovery_total: u64,
    manager_video_send_fail_total: u64,
    manager_fec_send_fail_total: u64,
    manager_audio_send_fail_total: u64,
    manager_max_video_queue_len: usize,
    manager_max_audio_queue_len: usize,
    last_manager_sample: ManagerTelemetrySnapshot,
    last_manager_sample_at: Instant,
    drop_delta_until_keyframe: bool,
}

#[derive(Clone, Copy, Default)]
struct ManagerTelemetrySnapshot {
    video_packets_in_total: u64,
    audio_packets_in_total: u64,
    video_packets_coalesced_total: u64,
    video_coalesce_events_total: u64,
    keyframe_req_queue_pressure_total: u64,
    keyframe_req_recovery_total: u64,
    keyframe_req_viewer_total: u64,
    video_dropped_for_recovery_total: u64,
    video_send_fail_total: u64,
    fec_send_fail_total: u64,
    audio_send_fail_total: u64,
}

unsafe impl Send for StreamManager {}
unsafe impl Sync for StreamManager {}

impl Drop for StreamManager {
    fn drop(&mut self) {
        log::info!("StreamManager dropping — cleaning up C++ host resources");
        unsafe {
            mello_sys::mello_stream_stop_audio(self.host);
            mello_sys::mello_stream_stop_host(self.host);
        }
    }
}

impl StreamManager {
    pub fn new(
        ctx: *mut mello_sys::MelloContext,
        host: *mut mello_sys::MelloStreamHost,
        sink: Arc<dyn PacketSink>,
        config: StreamConfig,
        video_rx: mpsc::Receiver<VideoPacket>,
        audio_rx: mpsc::Receiver<AudioPacket>,
    ) -> Self {
        Self {
            ctx,
            host,
            sink,
            fec_encoder: FecEncoder::new(0),
            video_seq: AtomicU16::new(0),
            audio_seq: AtomicU16::new(0),
            abr: AbrController::new(&config),
            config,
            input: Arc::new(InputPassthroughStub),
            video_rx,
            audio_rx,
            last_queue_keyframe_request: Instant::now()
                - Duration::from_secs(QUEUE_KEYFRAME_REQUEST_COOLDOWN_SECS),
            last_pacing_telemetry: None,
            last_pacing_sample_at: Instant::now(),
            manager_video_packets_in_total: 0,
            manager_audio_packets_in_total: 0,
            manager_video_packets_coalesced_total: 0,
            manager_video_coalesce_events_total: 0,
            manager_keyframe_req_queue_pressure_total: 0,
            manager_keyframe_req_recovery_total: 0,
            manager_keyframe_req_viewer_total: 0,
            manager_video_dropped_for_recovery_total: 0,
            manager_video_send_fail_total: 0,
            manager_fec_send_fail_total: 0,
            manager_audio_send_fail_total: 0,
            manager_max_video_queue_len: 0,
            manager_max_audio_queue_len: 0,
            last_manager_sample: ManagerTelemetrySnapshot::default(),
            last_manager_sample_at: Instant::now(),
            drop_delta_until_keyframe: false,
        }
    }

    fn calc_pacing_target_kbps(video_bitrate_kbps: u32, fec_n: usize) -> u32 {
        calc_stream_pacing_target_kbps(video_bitrate_kbps, fec_n, AUDIO_PACING_BUDGET_KBPS)
    }

    async fn refresh_pacing_target(&self) {
        let target = Self::calc_pacing_target_kbps(
            self.abr.current_bitrate_kbps(),
            self.abr.current_fec_n(),
        );
        self.sink.set_pacing_kbps(target).await;
    }

    async fn log_pacing_telemetry(&mut self) {
        let Some(now_stats) = self.sink.pacing_telemetry().await else {
            return;
        };
        let now = Instant::now();
        if let Some(prev) = self.last_pacing_telemetry {
            let elapsed_secs = now
                .duration_since(self.last_pacing_sample_at)
                .as_secs_f32()
                .max(0.001);
            let delta_bytes = now_stats.paced_bytes.saturating_sub(prev.paced_bytes);
            let delta_sleep_count = now_stats.sleep_count.saturating_sub(prev.sleep_count);
            let delta_sleep_ms = now_stats.sleep_ms_total.saturating_sub(prev.sleep_ms_total);
            let out_kbps = (delta_bytes as f32 * 8.0 / 1000.0) / elapsed_secs;
            log::info!(
                "Stream pacing: target_kbps={} out_kbps={:.1} paced_bytes_total={} sleep_count_total={} sleep_ms_total={} sleep_count_delta={} sleep_ms_delta={}",
                now_stats.target_kbps,
                out_kbps,
                now_stats.paced_bytes,
                now_stats.sleep_count,
                now_stats.sleep_ms_total,
                delta_sleep_count,
                delta_sleep_ms
            );
        }
        self.last_pacing_telemetry = Some(now_stats);
        self.last_pacing_sample_at = now;
    }

    fn manager_snapshot(&self) -> ManagerTelemetrySnapshot {
        ManagerTelemetrySnapshot {
            video_packets_in_total: self.manager_video_packets_in_total,
            audio_packets_in_total: self.manager_audio_packets_in_total,
            video_packets_coalesced_total: self.manager_video_packets_coalesced_total,
            video_coalesce_events_total: self.manager_video_coalesce_events_total,
            keyframe_req_queue_pressure_total: self.manager_keyframe_req_queue_pressure_total,
            keyframe_req_recovery_total: self.manager_keyframe_req_recovery_total,
            keyframe_req_viewer_total: self.manager_keyframe_req_viewer_total,
            video_dropped_for_recovery_total: self.manager_video_dropped_for_recovery_total,
            video_send_fail_total: self.manager_video_send_fail_total,
            fec_send_fail_total: self.manager_fec_send_fail_total,
            audio_send_fail_total: self.manager_audio_send_fail_total,
        }
    }

    async fn log_manager_telemetry(&mut self) {
        let now = Instant::now();
        let elapsed_secs = now
            .duration_since(self.last_manager_sample_at)
            .as_secs_f32()
            .max(0.001);
        let now_snapshot = self.manager_snapshot();
        let prev = self.last_manager_sample;

        let d_video_in = now_snapshot
            .video_packets_in_total
            .saturating_sub(prev.video_packets_in_total);
        let d_audio_in = now_snapshot
            .audio_packets_in_total
            .saturating_sub(prev.audio_packets_in_total);
        let d_coalesced = now_snapshot
            .video_packets_coalesced_total
            .saturating_sub(prev.video_packets_coalesced_total);
        let d_coalesce_events = now_snapshot
            .video_coalesce_events_total
            .saturating_sub(prev.video_coalesce_events_total);
        let d_recovery_drops = now_snapshot
            .video_dropped_for_recovery_total
            .saturating_sub(prev.video_dropped_for_recovery_total);
        let d_video_fail = now_snapshot
            .video_send_fail_total
            .saturating_sub(prev.video_send_fail_total);
        let d_fec_fail = now_snapshot
            .fec_send_fail_total
            .saturating_sub(prev.fec_send_fail_total);
        let d_audio_fail = now_snapshot
            .audio_send_fail_total
            .saturating_sub(prev.audio_send_fail_total);

        let video_queue_len = self.video_rx.len();
        let audio_queue_len = self.audio_rx.len();
        self.manager_max_video_queue_len = self.manager_max_video_queue_len.max(video_queue_len);
        self.manager_max_audio_queue_len = self.manager_max_audio_queue_len.max(audio_queue_len);

        log::info!(
            "Stream manager diag: video_in_hz={:.1} audio_in_hz={:.1} coalesced_hz={:.1} coalesce_events_delta={} recovery_drop_hz={:.1} recovery_mode={} keyframe_req_queue_total={} keyframe_req_recovery_total={} keyframe_req_viewer_total={} send_fail_video_delta={} send_fail_fec_delta={} send_fail_audio_delta={} video_queue_len={} audio_queue_len={} video_queue_max={} audio_queue_max={}",
            d_video_in as f32 / elapsed_secs,
            d_audio_in as f32 / elapsed_secs,
            d_coalesced as f32 / elapsed_secs,
            d_coalesce_events,
            d_recovery_drops as f32 / elapsed_secs,
            self.drop_delta_until_keyframe,
            now_snapshot.keyframe_req_queue_pressure_total,
            now_snapshot.keyframe_req_recovery_total,
            now_snapshot.keyframe_req_viewer_total,
            d_video_fail,
            d_fec_fail,
            d_audio_fail,
            video_queue_len,
            audio_queue_len,
            self.manager_max_video_queue_len,
            self.manager_max_audio_queue_len
        );

        self.last_manager_sample = now_snapshot;
        self.last_manager_sample_at = now;
    }

    pub fn abr(&mut self) -> &mut AbrController {
        &mut self.abr
    }

    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Main run loop — called from a dedicated tokio task after stream start.
    pub async fn run(&mut self, mut stop: oneshot::Receiver<()>) {
        log::info!("Stream manager run loop started");
        self.refresh_pacing_target().await;
        self.log_pacing_telemetry().await;
        let mut pacing_tick =
            tokio::time::interval(Duration::from_secs(PACING_TELEMETRY_INTERVAL_SECS));
        pacing_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut manager_tick =
            tokio::time::interval(Duration::from_secs(MANAGER_TELEMETRY_INTERVAL_SECS));
        manager_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = &mut stop => {
                    log::info!("Stream manager received stop signal");
                    break;
                }
                Some(pkt) = self.video_rx.recv() => {
                    self.handle_video(pkt).await;
                }
                Some(pkt) = self.audio_rx.recv() => {
                    self.handle_audio(pkt).await;
                }
                _ = pacing_tick.tick() => {
                    self.log_pacing_telemetry().await;
                }
                _ = manager_tick.tick() => {
                    self.log_manager_telemetry().await;
                }
                else => {
                    log::info!("Stream manager: packet channels closed");
                    break;
                }
            }
        }

        log::info!("Stream manager run loop exited");
    }

    fn request_host_keyframe(&mut self, reason: &str) -> bool {
        if self.last_queue_keyframe_request.elapsed()
            < Duration::from_secs(QUEUE_KEYFRAME_REQUEST_COOLDOWN_SECS)
        {
            return false;
        }
        unsafe {
            mello_sys::mello_stream_request_keyframe(self.host);
        }
        self.last_queue_keyframe_request = Instant::now();
        log::warn!(
            "Stream manager keyframe request: reason={} cooldown_sec={}",
            reason,
            QUEUE_KEYFRAME_REQUEST_COOLDOWN_SECS
        );
        true
    }

    async fn handle_video(&mut self, pkt: VideoPacket) {
        let (packet, coalesced) =
            coalesce_video_packet(pkt, &mut self.video_rx, MAX_VIDEO_COALESCE_DRAIN);
        self.manager_video_packets_in_total = self
            .manager_video_packets_in_total
            .saturating_add(1 + coalesced as u64);

        if coalesced > 0 {
            self.manager_video_packets_coalesced_total = self
                .manager_video_packets_coalesced_total
                .saturating_add(coalesced as u64);
            self.manager_video_coalesce_events_total =
                self.manager_video_coalesce_events_total.saturating_add(1);
            if coalesced <= 5 || coalesced.is_multiple_of(30) {
                log::warn!(
                    "Stream manager video coalesce: dropped_stale={} keep_keyframe={}",
                    coalesced,
                    packet.is_keyframe
                );
            }
            if !packet.is_keyframe && coalesced >= QUEUE_KEYFRAME_COALESCE_THRESHOLD {
                let requested_keyframe = self.request_host_keyframe("queue_pressure");
                if requested_keyframe {
                    self.manager_keyframe_req_queue_pressure_total = self
                        .manager_keyframe_req_queue_pressure_total
                        .saturating_add(1);
                }
                if coalesced >= QUEUE_RECOVERY_COALESCE_THRESHOLD && !self.drop_delta_until_keyframe
                {
                    self.drop_delta_until_keyframe = true;
                    log::warn!(
                        "Stream manager entering recovery mode: reason=queue_pressure dropped_stale={} hold_non_keyframe=true",
                        coalesced
                    );
                }
                log::warn!(
                    "Stream manager severe coalesce under pressure: reason=queue_pressure dropped_stale={} requested_keyframe={}",
                    coalesced,
                    requested_keyframe
                );
            }
        }
        if coalesced == MAX_VIDEO_COALESCE_DRAIN {
            log::warn!(
                "Stream manager video coalesce hit drain cap={} (preventing run-loop starvation)",
                MAX_VIDEO_COALESCE_DRAIN
            );
        }

        let seq = self.video_seq.fetch_add(1, Ordering::Relaxed);

        if packet.is_keyframe {
            if self.drop_delta_until_keyframe {
                self.drop_delta_until_keyframe = false;
                log::info!("Stream manager recovery mode cleared: reason=keyframe_received");
            }
            self.fec_encoder.reset();
        } else if self.drop_delta_until_keyframe {
            self.manager_video_dropped_for_recovery_total = self
                .manager_video_dropped_for_recovery_total
                .saturating_add(1);
            if self.request_host_keyframe("recovery_wait_keyframe") {
                self.manager_keyframe_req_recovery_total =
                    self.manager_keyframe_req_recovery_total.saturating_add(1);
            }
            return;
        }

        let fec_group_last = self.fec_encoder.is_enabled()
            && self.fec_encoder.pending_count() == self.fec_encoder.group_size() - 1;

        if let Some(parity) = self.fec_encoder.push(&packet.data) {
            let fec_packet = StreamPacket::fec(parity, seq);
            if let Err(e) = self.sink.send_video(&fec_packet).await {
                self.manager_fec_send_fail_total =
                    self.manager_fec_send_fail_total.saturating_add(1);
                log::warn!("Stream manager failed to send FEC packet: {}", e);
            }
        }

        let stream_packet =
            StreamPacket::video(packet.data, seq, packet.is_keyframe, fec_group_last);
        if let Err(e) = self.sink.send_video(&stream_packet).await {
            self.manager_video_send_fail_total =
                self.manager_video_send_fail_total.saturating_add(1);
            log::warn!("Stream manager failed to send video packet: {}", e);
        }
    }

    async fn handle_audio(&mut self, pkt: AudioPacket) {
        self.manager_audio_packets_in_total = self.manager_audio_packets_in_total.saturating_add(1);
        let seq = self.audio_seq.fetch_add(1, Ordering::Relaxed);
        let packet = StreamPacket::audio(pkt.data, seq);
        if let Err(e) = self.sink.send_audio(&packet).await {
            self.manager_audio_send_fail_total =
                self.manager_audio_send_fail_total.saturating_add(1);
            log::warn!("Stream manager failed to send audio packet: {}", e);
        }
    }

    /// Process an incoming control packet from a viewer.
    pub async fn handle_control_packet(
        &mut self,
        viewer_id: &str,
        packet: &StreamPacket,
    ) -> Option<super::abr::AbrChange> {
        if packet.ptype != PacketType::Control || packet.payload.is_empty() {
            return None;
        }

        let subtype = ControlSubtype::from_u8(packet.payload[0]);
        match subtype {
            Some(ControlSubtype::KeyframeRequest) => {
                self.manager_keyframe_req_viewer_total =
                    self.manager_keyframe_req_viewer_total.saturating_add(1);
                log::info!(
                    "Stream manager keyframe requested: reason=viewer_control viewer={}",
                    viewer_id
                );
                self.request_host_keyframe("viewer_control");
                None
            }
            Some(ControlSubtype::LossReport) => {
                if let Some(report) = LossReport::parse(&packet.payload) {
                    log::debug!(
                        "Loss report from {}: recv={} lost={} ({:.1}%) rx_kbps={}",
                        viewer_id,
                        report.packets_received,
                        report.packets_lost,
                        report.loss_ratio() * 100.0,
                        report.observed_rx_kbps.unwrap_or(0)
                    );
                    let change = self.abr.process_loss_report(viewer_id, &report);
                    if let Some(ref c) = change {
                        if let Some(new_br) = c.new_bitrate_kbps {
                            unsafe {
                                mello_sys::mello_stream_set_bitrate(self.host, new_br);
                            }
                        }
                        if let Some(new_fec) = c.new_fec_n {
                            self.fec_encoder.set_group_size(new_fec);
                        }
                        self.refresh_pacing_target().await;
                    }
                    change
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

fn coalesce_video_packet(
    packet: VideoPacket,
    video_rx: &mut mpsc::Receiver<VideoPacket>,
    max_drain: usize,
) -> (VideoPacket, usize) {
    let mut coalesced = 0usize;
    let mut newest_keyframe: Option<VideoPacket> = None;
    let mut newest_delta: Option<VideoPacket> = None;

    while coalesced < max_drain {
        let Ok(next) = video_rx.try_recv() else {
            break;
        };
        coalesced += 1;
        if next.is_keyframe {
            newest_keyframe = Some(next);
        } else {
            newest_delta = Some(next);
        }
    }

    // Prefer the newest keyframe if one was in the drain window (clean recovery
    // point). Otherwise prefer the newest delta — the reference chain is already
    // broken by the dropped intermediates, and the newest delta is temporally
    // closest to the encoder's current state, minimizing artifact duration until
    // the next keyframe arrives.
    if let Some(kf) = newest_keyframe {
        (kf, coalesced)
    } else if let Some(delta) = newest_delta {
        (delta, coalesced)
    } else {
        (packet, coalesced)
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::{coalesce_video_packet, VideoPacket, MAX_VIDEO_COALESCE_DRAIN};

    #[test]
    fn coalesce_video_packet_caps_drain_to_avoid_starvation() {
        let first = VideoPacket {
            data: vec![1],
            is_keyframe: false,
            timestamp: 1,
        };
        let (tx, mut rx) = mpsc::channel(256);
        for i in 0..200u64 {
            tx.try_send(VideoPacket {
                data: vec![2],
                is_keyframe: false,
                timestamp: i + 2,
            })
            .expect("queue should have room");
        }

        let (_picked, coalesced) = coalesce_video_packet(first, &mut rx, MAX_VIDEO_COALESCE_DRAIN);
        assert_eq!(coalesced, MAX_VIDEO_COALESCE_DRAIN);
        assert!(
            rx.try_recv().is_ok(),
            "queue should still contain pending frames"
        );
    }

    #[test]
    fn coalesce_video_packet_keeps_newest_delta_without_keyframe() {
        let first = VideoPacket {
            data: vec![1],
            is_keyframe: false,
            timestamp: 1,
        };
        let (tx, mut rx) = mpsc::channel(32);
        tx.try_send(VideoPacket {
            data: vec![2],
            is_keyframe: false,
            timestamp: 2,
        })
        .expect("queue should have room");
        tx.try_send(VideoPacket {
            data: vec![3],
            is_keyframe: false,
            timestamp: 3,
        })
        .expect("queue should have room");

        let (picked, coalesced) = coalesce_video_packet(first, &mut rx, MAX_VIDEO_COALESCE_DRAIN);
        assert_eq!(coalesced, 2);
        assert_eq!(picked.timestamp, 3);
        assert_eq!(picked.data, vec![3]);
    }
}
