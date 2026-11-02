//! The client side of the game clip player (`crate::clip_play`): the
//! commands, the state for the UI, and the stop on the teardown thread.

use crate::clip_play::{ClipPlayback, SendCtx};
use crate::events::Event;

impl super::Client {
    pub(super) async fn handle_play_game_clip(&mut self, source: String) {
        if self.viewer_state.is_some() || self.watch_join.is_some() {
            log::info!("[clip-play] {source} waits: a stream is watched");
            let _ = self.event_tx.send(Event::GameClipPlaybackClosed {
                reason: "Stop watching the stream to play a clip".into(),
            });
            return;
        }
        self.close_game_clip("");
        let ctx = SendCtx(self.voice.mello_ctx());
        if ctx.0.is_null() {
            let _ = self.event_tx.send(Event::GameClipPlaybackClosed {
                reason: "The media engine is not running".into(),
            });
            return;
        }
        // A URL costs one request: off the command loop.
        let opened = tokio::task::spawn_blocking(move || ClipPlayback::start(ctx, &source)).await;
        match opened {
            Ok(Ok(playback)) => self.clip_playback = Some(playback),
            Ok(Err(e)) => {
                log::warn!("[clip-play] the clip does not play: {e}");
                let _ = self.event_tx.send(Event::GameClipPlaybackClosed {
                    reason: "The clip does not load".into(),
                });
            }
            Err(e) => log::warn!("[clip-play] the open task failed: {e}"),
        }
    }

    pub(super) fn handle_set_game_clip_paused(&mut self, paused: bool) {
        let Some(playback) = &self.clip_playback else {
            return;
        };
        let state = playback.state();
        if !paused && state.ended {
            // Play again from the start.
            playback.seek(0);
        }
        playback.set_paused(paused);
    }

    pub(super) fn handle_seek_game_clip(&mut self, position_ms: u64) {
        if let Some(playback) = &self.clip_playback {
            playback.seek(position_ms);
        }
    }

    /// Stops the clip that plays, on the teardown thread (the stop joins
    /// libmello's threads). `reason` goes to the UI; empty: the user closed.
    pub(super) fn close_game_clip(&mut self, reason: &str) {
        let Some(playback) = self.clip_playback.take() else {
            return;
        };
        crate::stream::teardown::spawn("clip_player", move |_| drop(playback));
        let _ = self.event_tx.send(Event::GameClipPlaybackClosed {
            reason: reason.to_string(),
        });
    }

    /// Every 100 ms while a clip plays: its state when the UI must hear it.
    pub(super) fn clip_play_tick(&mut self) {
        let Some(playback) = self.clip_playback.as_mut() else {
            return;
        };
        let Some(s) = playback.poll_changed() else {
            return;
        };
        if s.failed {
            log::warn!("[clip-play] {} failed", playback.source);
            self.close_game_clip("The clip does not play");
            return;
        }
        let _ = self.event_tx.send(Event::GameClipPlayback {
            position_ms: s.position_ms,
            duration_ms: s.duration_ms,
            width: s.width,
            height: s.height,
            buffering: s.buffering,
            paused: s.paused,
            ended: s.ended,
        });
    }
}
