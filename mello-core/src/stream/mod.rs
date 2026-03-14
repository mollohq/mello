pub mod abr;
pub mod config;
pub mod error;
pub mod fec;
pub mod host;
pub mod input;
pub mod manager;
pub mod packet;
pub mod sink;
pub mod sink_p2p;
pub mod sink_sfu;
pub mod viewer;

pub use config::{Codec, QualityPreset, StreamConfig};
pub use error::StreamError;
pub use manager::StreamManager;
pub use packet::{PacketFlags, PacketType, StreamPacket};

/// Returns true if a HW encoder (NVENC/AMF/QSV) is available on this machine.
pub fn encoder_available(ctx: *mut mello_sys::MelloContext) -> bool {
    if ctx.is_null() {
        return false;
    }
    unsafe { mello_sys::mello_encoder_available(ctx) }
}
