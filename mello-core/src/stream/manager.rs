use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::time::MissedTickBehavior;

use super::config::StreamConfig;
use super::error::StreamError;
use super::input::{InputPassthrough, InputPassthroughStub};
use super::pacer::{calc_stream_pacing_target_kbps, PacingTelemetry};
use super::sink::{PacketSink, SinkVideoFeedback, SinkVideoFeedbackKind};

const PACING_TELEMETRY_INTERVAL_SECS: u64 = 2;
const MANAGER_TELEMETRY_INTERVAL_SECS: u64 = 1;
const MAX_VIDEO_COALESCE_DRAIN: usize = 32;
const QUEUE_KEYFRAME_COALESCE_THRESHOLD: usize = 2;
const QUEUE_RECOVERY_COALESCE_THRESHOLD: usize = 10;
const QUEUE_KEYFRAME_REQUEST_COOLDOWN_SECS: u64 = 2;
const VIEWER_KEYFRAME_REQUEST_COOLDOWN_MS: u64 = 500;
const FEEDBACK_POLL_INTERVAL_MS: u64 = 10;
const REMB_STALE_SECS: u64 = 3;
const REMB_INCREASE_MAX_PER_SEC: f32 = 0.05;

#[derive(Clone, Copy)]
struct ViewerRembState {
    bitrate_bps: u32,
    updated_at: Instant,
}

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
    manager_task: Option<tokio::task::JoinHandle<()>>,
}

impl StreamSession {
    pub fn new(
        session_id: String,
        mode: String,
        stop_tx: oneshot::Sender<()>,
        manager_task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            session_id,
            mode,
            stop_tx: Some(stop_tx),
            manager_task: Some(manager_task),
        }
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }

    /// Signal the manager and wait until it has stopped using host and sink resources.
    pub async fn stop_and_wait(mut self) {
        self.stop();
        if let Some(task) = self.manager_task.take() {
            if let Err(e) = task.await {
                log::warn!("Stream manager task join failed during shutdown: {}", e);
            }
        }
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The stream manager orchestrates the host-side streaming pipeline:
/// receives encoded access units from libmello and sends them through native RTP sinks.
pub struct StreamManager {
    #[allow(dead_code)]
    ctx: *mut mello_sys::MelloContext,
    host: *mut mello_sys::MelloStreamHost,
    sink: Arc<dyn PacketSink>,
    config: StreamConfig,
    #[allow(dead_code)]
    input: Arc<dyn InputPassthrough>,
    video_rx: tokio::sync::mpsc::Receiver<VideoPacket>,
    audio_rx: tokio::sync::mpsc::Receiver<AudioPacket>,
    current_bitrate_kbps: u32,
    min_bitrate_kbps: u32,
    max_bitrate_kbps: u32,
    viewer_remb: HashMap<String, ViewerRembState>,
    remb_increase_last_at: Instant,
    audio_seq: AtomicU16,
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
    manager_keyframe_req_feedback_total: u64,
    manager_video_dropped_for_recovery_total: u64,
    manager_video_chain_gap_total: u64,
    manager_video_send_fail_total: u64,
    manager_audio_stub_total: u64,
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
    keyframe_req_feedback_total: u64,
    video_dropped_for_recovery_total: u64,
    video_chain_gap_total: u64,
    video_send_fail_total: u64,
    audio_stub_total: u64,
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
        video_rx: tokio::sync::mpsc::Receiver<VideoPacket>,
        audio_rx: tokio::sync::mpsc::Receiver<AudioPacket>,
    ) -> Self {
        let max_bitrate_kbps = config.bitrate_kbps;
        let min_bitrate_kbps = StreamConfig::min_bitrate_kbps(config.codec);
        Self {
            ctx,
            host,
            sink,
            current_bitrate_kbps: max_bitrate_kbps,
            min_bitrate_kbps,
            max_bitrate_kbps,
            viewer_remb: HashMap::new(),
            remb_increase_last_at: Instant::now(),
            config,
            input: Arc::new(InputPassthroughStub),
            video_rx,
            audio_rx,
            audio_seq: AtomicU16::new(0),
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
            manager_keyframe_req_feedback_total: 0,
            manager_video_dropped_for_recovery_total: 0,
            manager_video_chain_gap_total: 0,
            manager_video_send_fail_total: 0,
            manager_audio_stub_total: 0,
            manager_max_video_queue_len: 0,
            manager_max_audio_queue_len: 0,
            last_manager_sample: ManagerTelemetrySnapshot::default(),
            last_manager_sample_at: Instant::now(),
            drop_delta_until_keyframe: false,
        }
    }

    fn calc_pacing_target_kbps(video_bitrate_kbps: u32) -> u32 {
        calc_stream_pacing_target_kbps(video_bitrate_kbps)
    }

    async fn refresh_pacing_target(&self) {
        let target = Self::calc_pacing_target_kbps(self.current_bitrate_kbps);
        self.sink.set_pacing_kbps(target).await;
    }

    fn clamp_bitrate_kbps(&self, kbps: u32) -> u32 {
        kbps.max(self.min_bitrate_kbps).min(self.max_bitrate_kbps)
    }

    fn apply_bitrate_kbps(&mut self, kbps: u32) -> bool {
        let clamped = self.clamp_bitrate_kbps(kbps);
        if clamped == self.current_bitrate_kbps {
            return false;
        }
        if !self.host.is_null() {
            unsafe {
                mello_sys::mello_stream_set_bitrate(self.host, clamped);
            }
        }
        log::info!(
            "Stream manager bitrate update: {} -> {} kbps",
            self.current_bitrate_kbps,
            clamped
        );
        self.current_bitrate_kbps = clamped;
        true
    }

    fn expire_stale_remb(&mut self, now: Instant) {
        self.viewer_remb
            .retain(|_, state| now.duration_since(state.updated_at).as_secs() <= REMB_STALE_SECS);
    }

    fn aggregate_fresh_remb_bps(&self, now: Instant) -> Option<u32> {
        self.viewer_remb
            .values()
            .filter(|state| now.duration_since(state.updated_at).as_secs() <= REMB_STALE_SECS)
            .map(|state| state.bitrate_bps)
            .min()
    }

    fn aggregate_remb_target_kbps(&self, now: Instant) -> Option<u32> {
        let min_bps = self.aggregate_fresh_remb_bps(now)?;
        Some(self.clamp_bitrate_kbps((min_bps / 1_000).max(1)))
    }

    async fn apply_remb_aggregate(&mut self, now: Instant) {
        self.expire_stale_remb(now);
        let Some(desired_kbps) = self.aggregate_remb_target_kbps(now) else {
            if self.apply_bitrate_kbps(self.max_bitrate_kbps) {
                self.refresh_pacing_target().await;
            }
            return;
        };

        let current_kbps = self.current_bitrate_kbps;
        if desired_kbps <= current_kbps {
            if self.apply_bitrate_kbps(desired_kbps) {
                self.refresh_pacing_target().await;
            }
            self.remb_increase_last_at = now;
            return;
        }

        let elapsed_secs = now
            .duration_since(self.remb_increase_last_at)
            .as_secs_f32()
            .max(0.0);
        let max_step_kbps = ((current_kbps as f32) * REMB_INCREASE_MAX_PER_SEC * elapsed_secs)
            .round()
            .max(1.0) as u32;
        let capped_kbps = current_kbps.saturating_add(max_step_kbps).min(desired_kbps);
        if self.apply_bitrate_kbps(capped_kbps) {
            self.refresh_pacing_target().await;
        }
        self.remb_increase_last_at = now;
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
            let out_kbps = (delta_bytes as f32 * 8.0 / 1000.0) / elapsed_secs;
            log::info!(
                "Stream RTP pacing: target_kbps={} out_kbps={:.1} tx_bytes_total={}",
                now_stats.target_kbps,
                out_kbps,
                now_stats.paced_bytes
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
            keyframe_req_feedback_total: self.manager_keyframe_req_feedback_total,
            video_dropped_for_recovery_total: self.manager_video_dropped_for_recovery_total,
            video_chain_gap_total: self.manager_video_chain_gap_total,
            video_send_fail_total: self.manager_video_send_fail_total,
            audio_stub_total: self.manager_audio_stub_total,
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
        let d_chain_gap = now_snapshot
            .video_chain_gap_total
            .saturating_sub(prev.video_chain_gap_total);
        let d_video_fail = now_snapshot
            .video_send_fail_total
            .saturating_sub(prev.video_send_fail_total);

        let video_queue_len = self.video_rx.len();
        let audio_queue_len = self.audio_rx.len();
        self.manager_max_video_queue_len = self.manager_max_video_queue_len.max(video_queue_len);
        self.manager_max_audio_queue_len = self.manager_max_audio_queue_len.max(audio_queue_len);

        log::info!(
            "Stream manager diag: video_in_hz={:.1} audio_in_hz={:.1} coalesced_hz={:.1} coalesce_events_delta={} recovery_drop_hz={:.1} chain_gap_hz={:.1} recovery_mode={} keyframe_req_queue_total={} keyframe_req_recovery_total={} keyframe_req_viewer_total={} keyframe_req_feedback_total={} send_fail_video_delta={} audio_stub_total={} video_queue_len={} audio_queue_len={} video_queue_max={} audio_queue_max={} bitrate_kbps={}",
            d_video_in as f32 / elapsed_secs,
            d_audio_in as f32 / elapsed_secs,
            d_coalesced as f32 / elapsed_secs,
            d_coalesce_events,
            d_recovery_drops as f32 / elapsed_secs,
            d_chain_gap as f32 / elapsed_secs,
            self.drop_delta_until_keyframe,
            now_snapshot.keyframe_req_queue_pressure_total,
            now_snapshot.keyframe_req_recovery_total,
            now_snapshot.keyframe_req_viewer_total,
            now_snapshot.keyframe_req_feedback_total,
            d_video_fail,
            now_snapshot.audio_stub_total,
            video_queue_len,
            audio_queue_len,
            self.manager_max_video_queue_len,
            self.manager_max_audio_queue_len,
            self.current_bitrate_kbps
        );

        self.last_manager_sample = now_snapshot;
        self.last_manager_sample_at = now;
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
        let mut feedback_tick =
            tokio::time::interval(Duration::from_millis(FEEDBACK_POLL_INTERVAL_MS));
        feedback_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

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
                _ = feedback_tick.tick() => {
                    self.drain_sink_events().await;
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

    async fn drain_sink_events(&mut self) {
        let now = Instant::now();
        while let Some(viewer_id) = self.sink.poll_viewer_joined().await {
            self.handle_viewer_joined(&viewer_id).await;
        }
        while let Some(viewer_id) = self.sink.poll_viewer_left().await {
            self.handle_viewer_left(&viewer_id).await;
        }
        while let Some(feedback) = self.sink.poll_video_feedback().await {
            self.handle_video_feedback(&feedback, now).await;
        }
        if !self.viewer_remb.is_empty() {
            self.apply_remb_aggregate(now).await;
        }
    }

    async fn handle_viewer_joined(&mut self, viewer_id: &str) {
        self.viewer_remb.remove(viewer_id);
        self.sink.on_viewer_joined(viewer_id).await;
        self.manager_keyframe_req_viewer_total =
            self.manager_keyframe_req_viewer_total.saturating_add(1);
        log::info!(
            "Stream manager viewer joined: {} — requesting IDR",
            viewer_id
        );
        self.request_host_keyframe_with_cooldown(
            "viewer_join",
            Duration::from_millis(VIEWER_KEYFRAME_REQUEST_COOLDOWN_MS),
        );
    }

    async fn handle_viewer_left(&mut self, viewer_id: &str) {
        self.viewer_remb.remove(viewer_id);
        self.sink.on_viewer_left(viewer_id).await;
        let _ = self.apply_remb_aggregate(Instant::now()).await;
    }

    async fn handle_video_feedback(&mut self, feedback: &SinkVideoFeedback, now: Instant) {
        match feedback.kind {
            SinkVideoFeedbackKind::Pli | SinkVideoFeedbackKind::LocalIdrNeeded => {
                self.manager_keyframe_req_feedback_total =
                    self.manager_keyframe_req_feedback_total.saturating_add(1);
                let reason = match feedback.kind {
                    SinkVideoFeedbackKind::Pli => "native_pli",
                    SinkVideoFeedbackKind::LocalIdrNeeded => "native_local_idr",
                    _ => unreachable!(),
                };
                if !self.drop_delta_until_keyframe {
                    self.drop_delta_until_keyframe = true;
                    log::info!(
                        "Stream manager holding delta frames until keyframe: reason={}",
                        reason
                    );
                }
                log::info!(
                    "Stream manager keyframe requested: reason={} viewer={}",
                    reason,
                    feedback.viewer_id
                );
                self.request_host_keyframe_with_cooldown(
                    reason,
                    Duration::from_millis(VIEWER_KEYFRAME_REQUEST_COOLDOWN_MS),
                );
            }
            SinkVideoFeedbackKind::Remb { bitrate_bps } => {
                if bitrate_bps == 0 {
                    return;
                }
                self.viewer_remb.insert(
                    feedback.viewer_id.clone(),
                    ViewerRembState {
                        bitrate_bps,
                        updated_at: now,
                    },
                );
                log::debug!(
                    "Stream manager REMB from {}: {} bps (active_viewers={})",
                    feedback.viewer_id,
                    bitrate_bps,
                    self.viewer_remb.len()
                );
                self.apply_remb_aggregate(now).await;
            }
        }
    }

    fn request_host_keyframe(&mut self, reason: &str) -> bool {
        self.request_host_keyframe_with_cooldown(
            reason,
            Duration::from_secs(QUEUE_KEYFRAME_REQUEST_COOLDOWN_SECS),
        )
    }

    fn request_host_keyframe_with_cooldown(&mut self, reason: &str, cooldown: Duration) -> bool {
        if self.last_queue_keyframe_request.elapsed() < cooldown {
            return false;
        }
        unsafe {
            mello_sys::mello_stream_request_keyframe(self.host);
        }
        self.last_queue_keyframe_request = Instant::now();
        log::warn!(
            "Stream manager keyframe request: reason={} cooldown_ms={}",
            reason,
            cooldown.as_millis()
        );
        true
    }

    async fn handle_video(&mut self, pkt: VideoPacket) {
        let coalesce = coalesce_video_packet(pkt, &mut self.video_rx, MAX_VIDEO_COALESCE_DRAIN);

        match coalesce {
            CoalesceOutcome::ChainGap { coalesced } => {
                self.manager_video_packets_in_total = self
                    .manager_video_packets_in_total
                    .saturating_add(1 + coalesced as u64);
                self.manager_video_packets_coalesced_total = self
                    .manager_video_packets_coalesced_total
                    .saturating_add(coalesced as u64);
                self.manager_video_coalesce_events_total =
                    self.manager_video_coalesce_events_total.saturating_add(1);
                self.manager_video_chain_gap_total =
                    self.manager_video_chain_gap_total.saturating_add(1);
                if !self.drop_delta_until_keyframe {
                    self.drop_delta_until_keyframe = true;
                    log::warn!(
                        "Stream manager entering recovery mode: reason=reference_chain_gap dropped_stale={}",
                        coalesced
                    );
                }
                if self.request_host_keyframe("reference_chain_gap") {
                    self.manager_keyframe_req_recovery_total =
                        self.manager_keyframe_req_recovery_total.saturating_add(1);
                }
            }
            CoalesceOutcome::Send { packet, coalesced } => {
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
                        if coalesced >= QUEUE_RECOVERY_COALESCE_THRESHOLD
                            && !self.drop_delta_until_keyframe
                        {
                            self.drop_delta_until_keyframe = true;
                            log::warn!(
                                "Stream manager entering recovery mode: reason=queue_pressure dropped_stale={} hold_non_keyframe=true",
                                coalesced
                            );
                        }
                    }
                }
                if coalesced == MAX_VIDEO_COALESCE_DRAIN {
                    log::warn!(
                        "Stream manager video coalesce hit drain cap={} (preventing run-loop starvation)",
                        MAX_VIDEO_COALESCE_DRAIN
                    );
                }

                if packet.is_keyframe {
                    if self.drop_delta_until_keyframe {
                        self.drop_delta_until_keyframe = false;
                        log::info!(
                            "Stream manager recovery mode cleared: reason=keyframe_received"
                        );
                    }
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

                match self
                    .sink
                    .send_video(&packet.data, packet.timestamp, packet.is_keyframe)
                    .await
                {
                    Ok(()) => {}
                    Err(e @ StreamError::SfuSendFailed(_)) => {
                        self.manager_video_send_fail_total =
                            self.manager_video_send_fail_total.saturating_add(1);
                        if !self.drop_delta_until_keyframe {
                            self.drop_delta_until_keyframe = true;
                            log::warn!(
                                "Stream manager entering recovery mode: reason=sfu_send_failed err={}",
                                e
                            );
                        }
                        if self.request_host_keyframe("sfu_send_failed") {
                            self.manager_keyframe_req_recovery_total =
                                self.manager_keyframe_req_recovery_total.saturating_add(1);
                        }
                        let n = self.manager_video_send_fail_total;
                        if n <= 3 || n.is_multiple_of(120) {
                            log::warn!("Stream manager failed to send video access unit: {}", e);
                        }
                    }
                    Err(e) => {
                        self.manager_video_send_fail_total =
                            self.manager_video_send_fail_total.saturating_add(1);
                        let n = self.manager_video_send_fail_total;
                        if n <= 3 || n.is_multiple_of(120) {
                            log::warn!("Stream manager failed to send video access unit: {}", e);
                        }
                    }
                }
            }
        }
    }

    async fn handle_audio(&mut self, pkt: AudioPacket) {
        self.manager_audio_packets_in_total = self.manager_audio_packets_in_total.saturating_add(1);
        let _ = self.audio_seq.fetch_add(1, Ordering::Relaxed);
        self.sink.send_audio_stub(pkt.data.len()).await;
        self.manager_audio_stub_total = self.manager_audio_stub_total.saturating_add(1);
    }
}

enum CoalesceOutcome {
    Send {
        packet: VideoPacket,
        coalesced: usize,
    },
    ChainGap {
        coalesced: usize,
    },
}

fn coalesce_video_packet(
    packet: VideoPacket,
    video_rx: &mut tokio::sync::mpsc::Receiver<VideoPacket>,
    max_drain: usize,
) -> CoalesceOutcome {
    let mut coalesced = 0usize;
    let (mut newest_keyframe, head_delta) = if packet.is_keyframe {
        (Some(packet), None)
    } else {
        (None, Some(packet))
    };

    while coalesced < max_drain {
        let Ok(next) = video_rx.try_recv() else {
            break;
        };
        coalesced += 1;
        if next.is_keyframe {
            newest_keyframe = Some(next);
        }
    }

    if let Some(kf) = newest_keyframe {
        return CoalesceOutcome::Send {
            packet: kf,
            coalesced,
        };
    }
    if coalesced > 0 {
        // Dropped reference frames without a keyframe recovery point — never forward a broken delta.
        return CoalesceOutcome::ChainGap { coalesced };
    }
    CoalesceOutcome::Send {
        packet: head_delta.expect("non-keyframe head exists when no keyframe was selected"),
        coalesced,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use super::{
        coalesce_video_packet, CoalesceOutcome, StreamManager, StreamSession, VideoPacket,
        ViewerRembState, MAX_VIDEO_COALESCE_DRAIN, REMB_STALE_SECS,
    };
    use crate::stream::config::{Codec, QualityPreset, StreamConfig};
    use crate::stream::error::StreamError;
    use crate::stream::sink::{
        NativeRtpTelemetry, PacketSink, SinkVideoFeedback, SinkVideoFeedbackKind,
    };

    struct FakeSink {
        video: Mutex<Vec<(Vec<u8>, u64, bool)>>,
        pacing_kbps: AtomicU32,
        feedback: Mutex<Vec<SinkVideoFeedback>>,
        joins: Mutex<Vec<String>>,
        audio_stub_bytes: AtomicU32,
    }

    impl FakeSink {
        fn new() -> Self {
            Self {
                video: Mutex::new(Vec::new()),
                pacing_kbps: AtomicU32::new(0),
                feedback: Mutex::new(Vec::new()),
                joins: Mutex::new(Vec::new()),
                audio_stub_bytes: AtomicU32::new(0),
            }
        }

        fn push_feedback(&self, feedback: SinkVideoFeedback) {
            self.feedback.lock().expect("lock").push(feedback);
        }
    }

    #[async_trait]
    impl PacketSink for FakeSink {
        async fn send_video(
            &self,
            annex_b: &[u8],
            capture_timestamp_us: u64,
            is_keyframe: bool,
        ) -> Result<(), StreamError> {
            self.video.lock().expect("lock").push((
                annex_b.to_vec(),
                capture_timestamp_us,
                is_keyframe,
            ));
            Ok(())
        }

        async fn send_audio_stub(&self, byte_len: usize) {
            self.audio_stub_bytes.fetch_add(
                u32::try_from(byte_len).unwrap_or(u32::MAX),
                Ordering::Relaxed,
            );
        }

        async fn set_pacing_kbps(&self, target_kbps: u32) {
            self.pacing_kbps.store(target_kbps, Ordering::Relaxed);
        }

        async fn native_rtp_telemetry(&self) -> Option<NativeRtpTelemetry> {
            Some(NativeRtpTelemetry {
                target_kbps: self.pacing_kbps.load(Ordering::Relaxed),
                tx_access_units_sent: self.video.lock().expect("lock").len() as u64,
                tx_access_units_dropped: 0,
                tx_bytes_sent: 0,
            })
        }

        async fn poll_video_feedback(&self) -> Option<SinkVideoFeedback> {
            let mut q = self.feedback.lock().expect("lock");
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        }

        async fn poll_viewer_joined(&self) -> Option<String> {
            let mut q = self.joins.lock().expect("lock");
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        }

        async fn poll_viewer_left(&self) -> Option<String> {
            None
        }

        async fn on_viewer_joined(&self, _viewer_id: &str) {}
        async fn on_viewer_left(&self, _viewer_id: &str) {}
    }

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

        let outcome = coalesce_video_packet(first, &mut rx, MAX_VIDEO_COALESCE_DRAIN);
        match outcome {
            CoalesceOutcome::ChainGap { coalesced } => {
                assert_eq!(coalesced, MAX_VIDEO_COALESCE_DRAIN)
            }
            _ => panic!("expected chain gap when draining deltas without keyframe"),
        }
        assert!(
            rx.try_recv().is_ok(),
            "queue should still contain pending frames"
        );
    }

    #[test]
    fn coalesce_video_packet_chain_gap_when_dropping_deltas_without_keyframe() {
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

        let outcome = coalesce_video_packet(first, &mut rx, MAX_VIDEO_COALESCE_DRAIN);
        match outcome {
            CoalesceOutcome::ChainGap { coalesced } => assert_eq!(coalesced, 2),
            _ => panic!("must not forward newest delta across a reference gap"),
        }
    }

    #[test]
    fn coalesce_video_packet_preserves_head_keyframe_before_delta_backlog() {
        let first = VideoPacket {
            data: vec![0x65],
            is_keyframe: true,
            timestamp: 1,
        };
        let (tx, mut rx) = mpsc::channel(32);
        for timestamp in 2..=4 {
            tx.try_send(VideoPacket {
                data: vec![0x41],
                is_keyframe: false,
                timestamp,
            })
            .expect("queue should have room");
        }

        let outcome = coalesce_video_packet(first, &mut rx, MAX_VIDEO_COALESCE_DRAIN);
        match outcome {
            CoalesceOutcome::Send { packet, coalesced } => {
                assert!(packet.is_keyframe);
                assert_eq!(packet.timestamp, 1);
                assert_eq!(coalesced, 3);
            }
            CoalesceOutcome::ChainGap { .. } => {
                panic!("head keyframe must remain a valid recovery point")
            }
        }
    }

    #[test]
    fn coalesce_video_packet_sends_single_delta_without_drain() {
        let packet = VideoPacket {
            data: vec![9],
            is_keyframe: false,
            timestamp: 42,
        };
        let (_tx, mut rx) = mpsc::channel(4);
        let outcome = coalesce_video_packet(packet, &mut rx, MAX_VIDEO_COALESCE_DRAIN);
        match outcome {
            CoalesceOutcome::Send { packet, coalesced } => {
                assert_eq!(coalesced, 0);
                assert_eq!(packet.timestamp, 42);
                assert_eq!(packet.data, vec![9]);
            }
            _ => panic!("expected send for lone delta"),
        }
    }

    #[test]
    fn fake_sink_preserves_au_bytes_and_timestamp() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let sink = Arc::new(FakeSink::new());
        rt.block_on(async {
            sink.send_video(&[0, 0, 1, 0x67], 1_234_567, true)
                .await
                .expect("send");
        });
        let sent = sink.video.lock().expect("lock");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, vec![0, 0, 1, 0x67]);
        assert_eq!(sent[0].1, 1_234_567);
        assert!(sent[0].2);
    }

    #[test]
    fn remb_aggregate_uses_minimum_active_target() {
        let config = StreamConfig::from_preset(QualityPreset::High, Codec::H264);
        let sink = Arc::new(FakeSink::new());
        let (_video_tx, video_rx) = mpsc::channel(4);
        let (_audio_tx, audio_rx) = mpsc::channel(4);
        let mut mgr = StreamManager::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            sink.clone(),
            config.clone(),
            video_rx,
            audio_rx,
        );
        let now = Instant::now();
        mgr.viewer_remb.insert(
            "a".to_string(),
            ViewerRembState {
                bitrate_bps: 8_000_000,
                updated_at: now,
            },
        );
        mgr.viewer_remb.insert(
            "b".to_string(),
            ViewerRembState {
                bitrate_bps: 3_000_000,
                updated_at: now,
            },
        );
        assert_eq!(mgr.aggregate_remb_target_kbps(now), Some(3_000));
        std::mem::forget(mgr);
    }

    #[test]
    fn stale_remb_targets_are_ignored() {
        let config = StreamConfig::from_preset(QualityPreset::High, Codec::H264);
        let sink = Arc::new(FakeSink::new());
        let (_video_tx, video_rx) = mpsc::channel(4);
        let (_audio_tx, audio_rx) = mpsc::channel(4);
        let mut mgr = StreamManager::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            sink.clone(),
            config,
            video_rx,
            audio_rx,
        );
        let now = Instant::now();
        mgr.viewer_remb.insert(
            "stale".to_string(),
            ViewerRembState {
                bitrate_bps: 1_000_000,
                updated_at: now - Duration::from_secs(REMB_STALE_SECS + 1),
            },
        );
        assert_eq!(mgr.aggregate_fresh_remb_bps(now), None);
        std::mem::forget(mgr);
    }

    #[test]
    fn remb_decrease_applies_immediately() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let config = StreamConfig::from_preset(QualityPreset::High, Codec::H264);
        let sink = Arc::new(FakeSink::new());
        let (_video_tx, video_rx) = mpsc::channel(4);
        let (_audio_tx, audio_rx) = mpsc::channel(4);
        let mut mgr = StreamManager::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            sink.clone(),
            config,
            video_rx,
            audio_rx,
        );
        mgr.current_bitrate_kbps = 8_000;
        let now = Instant::now();
        mgr.viewer_remb.insert(
            "viewer".to_string(),
            ViewerRembState {
                bitrate_bps: 3_000_000,
                updated_at: now,
            },
        );

        rt.block_on(mgr.apply_remb_aggregate(now));

        assert_eq!(mgr.current_bitrate_kbps, 3_000);
        std::mem::forget(mgr);
    }

    #[test]
    fn remb_increase_is_rate_limited() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let config = StreamConfig::from_preset(QualityPreset::High, Codec::H264);
        let sink = Arc::new(FakeSink::new());
        let (_video_tx, video_rx) = mpsc::channel(4);
        let (_audio_tx, audio_rx) = mpsc::channel(4);
        let mut mgr = StreamManager::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            sink.clone(),
            config,
            video_rx,
            audio_rx,
        );
        mgr.current_bitrate_kbps = 3_000;
        let now = Instant::now();
        mgr.remb_increase_last_at = now - Duration::from_secs(1);
        mgr.viewer_remb.insert(
            "viewer".to_string(),
            ViewerRembState {
                bitrate_bps: 8_000_000,
                updated_at: now,
            },
        );

        rt.block_on(mgr.apply_remb_aggregate(now));

        assert_eq!(mgr.current_bitrate_kbps, 3_150);
        std::mem::forget(mgr);
    }

    #[test]
    fn remb_table_driven_aggregation() {
        struct Case {
            name: &'static str,
            targets: &'static [u32],
            expected_kbps: Option<u32>,
        }

        let now = Instant::now();
        let cases = [
            Case {
                name: "single_viewer",
                targets: &[4_500_000],
                expected_kbps: Some(4_500),
            },
            Case {
                name: "minimum_of_two",
                targets: &[8_000_000, 2_500_000],
                expected_kbps: Some(2_500),
            },
            Case {
                name: "empty_after_expiry",
                targets: &[],
                expected_kbps: None,
            },
        ];

        for case in cases {
            let config = StreamConfig::from_preset(QualityPreset::High, Codec::H264);
            let sink = Arc::new(FakeSink::new());
            let (_video_tx, video_rx) = mpsc::channel(4);
            let (_audio_tx, audio_rx) = mpsc::channel(4);
            let mut mgr = StreamManager::new(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                sink,
                config,
                video_rx,
                audio_rx,
            );
            for (idx, target) in case.targets.iter().enumerate() {
                mgr.viewer_remb.insert(
                    format!("viewer-{idx}"),
                    ViewerRembState {
                        bitrate_bps: *target,
                        updated_at: now,
                    },
                );
            }
            assert_eq!(
                mgr.aggregate_remb_target_kbps(now),
                case.expected_kbps,
                "case {} failed",
                case.name
            );
            std::mem::forget(mgr);
        }
    }

    #[test]
    fn last_remb_viewer_leave_restores_max_bitrate_and_pacing() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let config = StreamConfig::from_preset(QualityPreset::High, Codec::H264);
        let max_bitrate_kbps = config.bitrate_kbps;
        let sink = Arc::new(FakeSink::new());
        let (_video_tx, video_rx) = mpsc::channel(4);
        let (_audio_tx, audio_rx) = mpsc::channel(4);
        let mut mgr = StreamManager::new(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            sink.clone(),
            config,
            video_rx,
            audio_rx,
        );
        mgr.current_bitrate_kbps = 3_000;
        mgr.viewer_remb.insert(
            "last-viewer".to_string(),
            ViewerRembState {
                bitrate_bps: 3_000_000,
                updated_at: Instant::now(),
            },
        );

        rt.block_on(mgr.handle_viewer_left("last-viewer"));

        assert!(mgr.viewer_remb.is_empty());
        assert_eq!(mgr.current_bitrate_kbps, max_bitrate_kbps);
        assert_eq!(
            sink.pacing_kbps.load(Ordering::Relaxed),
            StreamManager::calc_pacing_target_kbps(max_bitrate_kbps)
        );
        std::mem::forget(mgr);
    }

    #[test]
    fn stop_and_wait_completes_before_peer_teardown_continues() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let order = Arc::new(Mutex::new(Vec::new()));

        rt.block_on(async {
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            let manager_order = Arc::clone(&order);
            let manager_task = tokio::spawn(async move {
                let _ = stop_rx.await;
                manager_order.lock().expect("lock").push("manager_exit");
            });
            let session = StreamSession::new(
                "session".to_string(),
                "p2p".to_string(),
                stop_tx,
                manager_task,
            );

            session.stop_and_wait().await;
            order.lock().expect("lock").push("peer_destroy");
        });

        assert_eq!(
            *order.lock().expect("lock"),
            vec!["manager_exit", "peer_destroy"]
        );
    }

    #[test]
    fn feedback_identity_preserved_per_viewer() {
        let sink = Arc::new(FakeSink::new());
        sink.push_feedback(SinkVideoFeedback {
            viewer_id: "viewer-2".to_string(),
            kind: SinkVideoFeedbackKind::Pli,
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let fb = rt.block_on(async { sink.poll_video_feedback().await });
        assert_eq!(
            fb,
            Some(SinkVideoFeedback {
                viewer_id: "viewer-2".to_string(),
                kind: SinkVideoFeedbackKind::Pli,
            })
        );
    }

    #[test]
    fn pacing_target_propagates_to_sink() {
        let sink = Arc::new(FakeSink::new());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            sink.set_pacing_kbps(5_500).await;
        });
        assert_eq!(sink.pacing_kbps.load(Ordering::Relaxed), 5_500);
    }

    #[test]
    fn audio_is_stubbed_not_sent_on_stream_datachannel() {
        let sink = Arc::new(FakeSink::new());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            sink.send_audio_stub(256).await;
        });
        assert_eq!(sink.audio_stub_bytes.load(Ordering::Relaxed), 256);
        assert!(sink.video.lock().expect("lock").is_empty());
    }
}
