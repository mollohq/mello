//! The game clip upload (plans/GAME-CLIPS.md 6.3 to 6.5). F8 writes an MP4
//! and a poster to disk. This module posts the clip to the crew and puts both
//! files in the bucket, in the background, while the game runs.
//!
//! - `worker`: one thread at low priority, one request at a time:
//!   `post_clip`, the poster, the MP4 in 5 MiB parts, the complete.
//! - `queue`: the persisted queue with the upload id and the done parts, so
//!   a restart goes on from the last part.
//! - `pace`: the rate cap. No cap with no game, 3 Mbps in a game, the
//!   stream's budget while the user streams (`stream/upload_budget.rs`).
//! - `http`: the blocking RPCs and PUTs.

mod http;
#[cfg(test)]
mod live_test;
mod pace;
mod queue;
mod worker;

pub use pace::{pace, UploadPace, GAME_RATE_BPS};
pub use queue::PendingClip;
pub(crate) use worker::UploadWorker;
