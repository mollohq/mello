use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use super::abr::AbrController;
use super::config::StreamConfig;
use super::fec::FecEncoder;
use super::input::InputPassthroughStub;
use super::packet::{ControlSubtype, LossReport, PacketType, StreamPacket};
use super::sink::PacketSink;

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

    /// Stop the stream session. The manager run loop will exit.
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
/// polls encoded packets from libmello, applies FEC, and sends through the sink.
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
    input: InputPassthroughStub,
}

unsafe impl Send for StreamManager {}
unsafe impl Sync for StreamManager {}

impl StreamManager {
    pub fn new(
        ctx: *mut mello_sys::MelloContext,
        host: *mut mello_sys::MelloStreamHost,
        sink: Arc<dyn PacketSink>,
        config: StreamConfig,
    ) -> Self {
        let fec_n = config.fec_n;
        Self {
            ctx,
            host,
            sink,
            fec_encoder: FecEncoder::new(fec_n),
            video_seq: AtomicU16::new(0),
            audio_seq: AtomicU16::new(0),
            abr: AbrController::new(&config),
            config,
            input: InputPassthroughStub,
        }
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
        let mut poll_interval = tokio::time::interval(Duration::from_millis(1));

        loop {
            tokio::select! {
                _ = &mut stop => {
                    log::info!("Stream manager received stop signal");
                    break;
                }
                _ = poll_interval.tick() => {
                    self.poll_video().await;
                    self.poll_audio().await;
                }
            }
        }

        log::info!("Stream manager run loop exited");
    }

    async fn poll_video(&mut self) {
        let mut buf = [0u8; 256 * 1024]; // 256KB max encoded frame
        let mut is_keyframe = false;

        let n = unsafe {
            mello_sys::mello_stream_get_video_packet(
                self.host,
                buf.as_mut_ptr(),
                buf.len() as i32,
                &mut is_keyframe,
            )
        };
        if n <= 0 {
            return;
        }

        let payload = buf[..n as usize].to_vec();
        let seq = self.video_seq.fetch_add(1, Ordering::Relaxed);

        // FEC group boundary resets on keyframe
        if is_keyframe {
            self.fec_encoder.reset();
        }

        // Determine if this is the last packet in the FEC group
        // (we track this by checking if the next push would complete the group)
        let group_pos = seq as usize % self.fec_encoder.group_size();
        let fec_group_last = group_pos == self.fec_encoder.group_size() - 1;

        let packet = StreamPacket::video(payload.clone(), seq, is_keyframe, fec_group_last);

        // Send the data packet first
        let _ = self.sink.send_video(&packet).await;

        // FEC: accumulate and send parity when group completes
        if let Some(parity) = self.fec_encoder.push(&payload) {
            let fec_packet = StreamPacket::fec(parity, seq);
            let _ = self.sink.send_video(&fec_packet).await;
        }
    }

    async fn poll_audio(&mut self) {
        let mut buf = [0u8; 4000]; // Opus packets are small

        let n = unsafe {
            mello_sys::mello_stream_get_audio_packet(
                self.host,
                buf.as_mut_ptr(),
                buf.len() as i32,
            )
        };
        if n <= 0 {
            return;
        }

        let payload = buf[..n as usize].to_vec();
        let seq = self.audio_seq.fetch_add(1, Ordering::Relaxed);
        let packet = StreamPacket::audio(payload, seq);

        // No FEC for audio — Opus has built-in PLC
        let _ = self.sink.send_audio(&packet).await;
    }

    /// Process an incoming control packet from a viewer.
    pub fn handle_control_packet(
        &mut self,
        viewer_id: &str,
        packet: &StreamPacket,
    ) -> Option<super::abr::BitrateChange> {
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
                        "Loss report from {}: recv={} lost={} ({:.1}%)",
                        viewer_id,
                        report.packets_received,
                        report.packets_lost,
                        report.loss_ratio() * 100.0
                    );
                    let change = self.abr.process_loss_report(viewer_id, &report);
                    if let Some(ref c) = change {
                        unsafe {
                            mello_sys::mello_stream_set_bitrate(self.host, c.new_bitrate_kbps);
                        }
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
