pub mod config;
pub mod congestion;
pub mod error;
pub mod host;
pub mod input;
pub mod manager;
pub mod pacer;
pub mod rtp_peer;
pub mod sink;
pub mod sink_p2p;
pub mod sink_sfu;

pub use config::{Codec, QualityPreset, StreamConfig};
pub use error::StreamError;
pub use manager::StreamManager;

/// Returns true if a HW encoder (NVENC/AMF/QSV) is available on this machine.
///
/// # Safety
/// `ctx` must be a valid, non-null `MelloContext` pointer returned by libmello.
pub unsafe fn encoder_available(ctx: *mut mello_sys::MelloContext) -> bool {
    if ctx.is_null() {
        return false;
    }
    mello_sys::mello_encoder_available(ctx)
}
