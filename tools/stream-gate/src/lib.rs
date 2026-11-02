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
//! `replay` is the game clip lane (plans/GAME-CLIPS.md 10.3): the same clip
//! and game audio through the replay buffer into MP4 clips, each read back
//! and checked. Run it with `scripts/replay-gate.sh`.

pub mod audio;
pub mod clip;
pub mod code;
pub mod load;
pub mod metrics;
pub mod netmodel;
pub mod profile;
pub mod replay;
pub mod report;
pub mod score;
pub mod session;
pub mod sfu;
pub mod shim;
