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

pub mod clip;
pub mod code;
pub mod metrics;
pub mod netmodel;
pub mod profile;
pub mod report;
pub mod score;
pub mod session;
pub mod sfu;
pub mod shim;
