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
    cmd_tx: tokio_mpsc::UnboundedSender<Command>,
    mut mixer: FrameMixer,
) -> InjectLoopHandle {
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    rt.spawn(async move {
        if cmd_tx.send(Command::StartVoiceCaptureInject).is_err() {
            return;
        }

        let mut tick = tokio::time::interval(tokio::time::Duration::from_millis(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = tick.tick() => {
                    let Some(frame) = mixer.next_frame() else { break };
                    if cmd_tx.send(Command::InjectCaptureFrame { samples: frame }).is_err() {
                        break;
                    }
                }
            }
        }

        let _ = cmd_tx.send(Command::StopVoiceCaptureInject);
    });

    InjectLoopHandle {
        stop_tx: Some(stop_tx),
    }
}
