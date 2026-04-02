use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use super::abr::AbrController;
use super::config::StreamConfig;
use super::fec::FecEncoder;
use super::input::{InputPassthrough, InputPassthroughStub};
use super::packet::{ControlSubtype, LossReport, PacketType, StreamPacket};
use super::sink::PacketSink;

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
    video_rx: mpsc::UnboundedReceiver<VideoPacket>,
    audio_rx: mpsc::UnboundedReceiver<AudioPacket>,
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
        video_rx: mpsc::UnboundedReceiver<VideoPacket>,
        audio_rx: mpsc::UnboundedReceiver<AudioPacket>,
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
            }
        }

        log::info!("Stream manager run loop exited");
    }

    async fn handle_video(&mut self, pkt: VideoPacket) {
        let seq = self.video_seq.fetch_add(1, Ordering::Relaxed);

        if pkt.is_keyframe {
            self.fec_encoder.reset();
        }

        let fec_group_last = self.fec_encoder.is_enabled()
            && self.fec_encoder.pending_count() == self.fec_encoder.group_size() - 1;

        let packet = StreamPacket::video(pkt.data.clone(), seq, pkt.is_keyframe, fec_group_last);
        let _ = self.sink.send_video(&packet).await;

        if let Some(parity) = self.fec_encoder.push(&pkt.data) {
            let fec_packet = StreamPacket::fec(parity, seq);
            let _ = self.sink.send_video(&fec_packet).await;
        }
    }

    async fn handle_audio(&mut self, pkt: AudioPacket) {
        let seq = self.audio_seq.fetch_add(1, Ordering::Relaxed);
        let packet = StreamPacket::audio(pkt.data, seq);
        let _ = self.sink.send_audio(&packet).await;
    }

    /// Process an incoming control packet from a viewer.
    pub fn handle_control_packet(
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
                        "Loss report from {}: recv={} lost={} ({:.1}%)",
                        viewer_id,
                        report.packets_received,
                        report.packets_lost,
                        report.loss_ratio() * 100.0
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
