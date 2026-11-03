//! Streaming quality gate (plans/streaming-quality.md stage 1).
//!
//! A deterministic reference clip (`clip`) plays through the real host path
//! (reference source, preprocessor, encoder, RTP sender, mello-core manager
//! and SFU sink), an impairment shim (`shim`, `netmodel`), the real SFU in
//! test mode (`sfu`), the shim again, and the real viewer path (RTP receiver
//! session, decoder, present). The viewer reads the burned-in code (`code`)
//! of every presented frame. `metrics` and `score` turn that into delivery,
//! latency and visual scores; `report` prints them against the frozen
//! baseline and checks the gates. Run it with `scripts/stream-gate.sh`.
//!
//! `replay` is the game clip lane (CLIPS.md 15.13): the same clip
//! and game audio through the replay buffer into MP4 clips, each read back
//! and checked. Run it with `scripts/replay-gate.sh`. Its streamed playback
//! row (`playback`) plays a clip over HTTP through a delay and rate shim
//! (`http_shim`), mello-core's range reader and libmello's clip player.

pub mod audio;
pub mod clip;
pub mod code;
pub mod http_shim;
pub mod load;
pub mod metrics;
pub mod netmodel;
pub mod playback;
pub mod profile;
pub mod replay;
pub mod report;
pub mod score;
pub mod session;
pub mod sfu;
pub mod shim;
