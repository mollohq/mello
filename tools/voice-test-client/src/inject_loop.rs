use std::sync::mpsc;

use mello_core::Command;
use tokio::runtime::Handle;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::wav_player::FrameMixer;

pub struct InjectLoopHandle {
    stop_tx: Option<oneshot::Sender<()>>,
}

impl InjectLoopHandle {
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

pub fn start_inject_loop(
    rt: &Handle,
    cmd_tx: tokio_mpsc::Sender<Command>,
    mut mixer: FrameMixer,
    status_tx: mpsc::Sender<String>,
) -> InjectLoopHandle {
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    rt.spawn(async move {
        if cmd_tx.send(Command::StartVoiceCaptureInject).await.is_err() {
            let _ = status_tx.send("failed to start inject mode: command channel closed".to_string());
            return;
        }

        let _ = status_tx.send("capture injection started".to_string());
        let mut tick = tokio::time::interval(tokio::time::Duration::from_millis(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = &mut stop_rx => {
                    let _ = status_tx.send("capture injection stopping".to_string());
                    break;
                }
                _ = tick.tick() => {
                    let Some(frame) = mixer.next_frame() else {
                        let _ = status_tx.send("capture injection finished (source exhausted)".to_string());
                        break;
                    };
                    if cmd_tx.send(Command::InjectCaptureFrame { samples: frame }).await.is_err() {
                        let _ = status_tx.send("capture injection stopped: command channel closed".to_string());
                        break;
                    }
                }
            }
        }

        let _ = cmd_tx.send(Command::StopVoiceCaptureInject).await;
    });

    InjectLoopHandle {
        stop_tx: Some(stop_tx),
    }
}
