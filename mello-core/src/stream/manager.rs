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
            last_queue_keyframe_request: Instant::now() - Duration::from_secs(5),
            last_pacing_telemetry: None,
            last_pacing_sample_at: Instant::now(),
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
                else => {
                    log::info!("Stream manager: packet channels closed");
                    break;
                }
            }
        }

        log::info!("Stream manager run loop exited");
    }

    async fn handle_video(&mut self, pkt: VideoPacket) {
        let mut packet = pkt;
        let mut coalesced = 0usize;
        let mut newest_keyframe: Option<VideoPacket> = None;

        while let Ok(next) = self.video_rx.try_recv() {
            coalesced += 1;
            if next.is_keyframe {
                newest_keyframe = Some(next);
            } else if newest_keyframe.is_none() {
                packet = next;
            }
        }

        if let Some(kf) = newest_keyframe {
            packet = kf;
        }

        if coalesced > 0 {
            if coalesced <= 5 || coalesced.is_multiple_of(30) {
                log::warn!(
                    "Stream manager video coalesce: dropped_stale={} keep_keyframe={}",
                    coalesced,
                    packet.is_keyframe
                );
            }
            if !packet.is_keyframe
                && self.last_queue_keyframe_request.elapsed() >= Duration::from_secs(1)
            {
                unsafe {
                    mello_sys::mello_stream_request_keyframe(self.host);
                }
                self.last_queue_keyframe_request = Instant::now();
                log::warn!("Stream manager coalesced deltas under pressure: requested keyframe");
            }
        }

        let seq = self.video_seq.fetch_add(1, Ordering::Relaxed);

        if packet.is_keyframe {
            self.fec_encoder.reset();
        }

        let fec_group_last = self.fec_encoder.is_enabled()
            && self.fec_encoder.pending_count() == self.fec_encoder.group_size() - 1;

        if let Some(parity) = self.fec_encoder.push(&packet.data) {
            let fec_packet = StreamPacket::fec(parity, seq);
            let _ = self.sink.send_video(&fec_packet).await;
        }

        let stream_packet =
            StreamPacket::video(packet.data, seq, packet.is_keyframe, fec_group_last);
        let _ = self.sink.send_video(&stream_packet).await;
    }

    async fn handle_audio(&mut self, pkt: AudioPacket) {
        let seq = self.audio_seq.fetch_add(1, Ordering::Relaxed);
        let packet = StreamPacket::audio(pkt.data, seq);
        let _ = self.sink.send_audio(&packet).await;
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
                log::info!("Keyframe request from viewer {}", viewer_id);
                unsafe {
                    mello_sys::mello_stream_request_keyframe(self.host);
                }
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
