//! Safe wrappers over the libmello RTP peer video C ABI that the SFU
//! connection uses.
//!
//! Ownership of [`MelloPeerConnection`] stays with the caller: these helpers never
//! destroy peers. Every function documents the pointer validity it requires.

use std::ptr::NonNull;

use mello_sys::{MelloPeerConnection, MelloRtpVideoAccessUnitInfo};
use thiserror::Error;

/// Matches `#define MELLO_PEER_VIDEO_RECV_ERROR INT32_MIN` in mello.h.
///
/// Bindgen does not emit this macro; keep it aligned with the header.
pub const VIDEO_RECV_ERROR: i32 = i32::MIN;

/// Maximum Annex-B access unit size accepted by the native receiver
/// (`RtpVideoReceiverSession::kMaxOutputBytes` in libmello).
pub const MAX_ACCESS_UNIT_BYTES: usize = 4 * 1024 * 1024;

/// Native peer media role mirrored from `MelloPeerMediaRole` in mello.h.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerMediaRole {
    Voice,
    StreamHost,
    StreamViewer,
}

impl PeerMediaRole {
    /// Map the `media_role` field from [`MelloRtpVideoStats`].
    pub fn from_stats_byte(role: u8) -> Self {
        match role {
            x if x == mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_HOST as u8 => {
                Self::StreamHost
            }
            x if x == mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER as u8 => {
                Self::StreamViewer
            }
            _ => Self::Voice,
        }
    }
}

/// Errors surfaced by the RTP peer wrapper layer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RtpPeerError {
    #[error("MelloContext pointer is null")]
    NullContext,

    #[error("peer id contains interior nul bytes")]
    InvalidPeerId,

    #[error("native peer creation failed")]
    CreateFailed,

    #[error("MelloPeerConnection pointer is null")]
    NullPeer,

    #[error("video access unit receive failed")]
    RecvFailed,

    #[error(
        "required access unit size {required} bytes exceeds native bound {MAX_ACCESS_UNIT_BYTES}"
    )]
    AccessUnitTooLarge { required: i32 },

    #[error("invalid parameter")]
    InvalidParam,

    #[error("native transport call failed")]
    TransportFailed,

    #[error("RTP sender queue full or awaiting IDR")]
    Backpressure,
}

/// Metadata for one received Annex-B H.264 access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedAccessUnit {
    pub is_idr: bool,
    pub rtp_timestamp: u32,
    pub capture_timestamp_us: u64,
}

/// Poll one complete received access unit into `buffer`.
///
/// Grows `buffer` in place when the queued unit is larger than the current
/// capacity, retaining existing capacity on success. Growth is capped at
/// [`MAX_ACCESS_UNIT_BYTES`].
///
/// `peer` must be a valid `MelloPeerConnection` pointer until this call returns.
pub fn poll_received_access_unit(
    peer: NonNull<MelloPeerConnection>,
    buffer: &mut Vec<u8>,
) -> Result<Option<ReceivedAccessUnit>, RtpPeerError> {
    loop {
        let mut info = zero_access_unit_info();
        let rc = unsafe {
            mello_sys::mello_peer_video_recv_access_unit(
                peer.as_ptr(),
                buffer.as_mut_ptr(),
                i32::try_from(buffer.len()).map_err(|_| RtpPeerError::RecvFailed)?,
                &mut info,
            )
        };

        match classify_recv_result(rc)? {
            RecvPoll::Empty => return Ok(None),
            RecvPoll::Failed => return Err(RtpPeerError::RecvFailed),
            RecvPoll::TooSmall { required } => {
                grow_recv_buffer(buffer, required)?;
            }
            RecvPoll::Ready { bytes } => {
                buffer.truncate(bytes);
                return Ok(Some(ReceivedAccessUnit {
                    is_idr: info.is_idr != 0,
                    rtp_timestamp: info.rtp_timestamp,
                    capture_timestamp_us: info.capture_timestamp_us,
                }));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecvPoll {
    Empty,
    Failed,
    TooSmall { required: i32 },
    Ready { bytes: usize },
}

fn classify_recv_result(rc: i32) -> Result<RecvPoll, RtpPeerError> {
    if rc == 0 {
        return Ok(RecvPoll::Empty);
    }
    if rc == VIDEO_RECV_ERROR {
        return Ok(RecvPoll::Failed);
    }
    if rc < 0 {
        let required = rc.checked_neg().ok_or(RtpPeerError::RecvFailed)?;
        return Ok(RecvPoll::TooSmall { required });
    }
    let bytes = usize::try_from(rc).map_err(|_| RtpPeerError::RecvFailed)?;
    Ok(RecvPoll::Ready { bytes })
}

fn recv_capacity_for(required: i32) -> Result<usize, RtpPeerError> {
    if required <= 0 {
        return Err(RtpPeerError::RecvFailed);
    }
    let required_usize =
        usize::try_from(required).map_err(|_| RtpPeerError::AccessUnitTooLarge { required })?;
    if required_usize > MAX_ACCESS_UNIT_BYTES {
        return Err(RtpPeerError::AccessUnitTooLarge { required });
    }
    Ok(required_usize)
}

fn grow_recv_buffer(buffer: &mut Vec<u8>, required: i32) -> Result<(), RtpPeerError> {
    let capacity = recv_capacity_for(required)?;
    if buffer.len() < capacity {
        buffer.resize(capacity, 0);
    }
    Ok(())
}

fn zero_access_unit_info() -> MelloRtpVideoAccessUnitInfo {
    MelloRtpVideoAccessUnitInfo {
        size: 0,
        is_idr: 0,
        rtp_timestamp: 0,
        capture_timestamp_us: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_recv_error_matches_header() {
        assert_eq!(VIDEO_RECV_ERROR, i32::MIN);
    }

    #[test]
    fn peer_media_role_maps_stats_bytes() {
        assert_eq!(
            PeerMediaRole::from_stats_byte(
                mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_VOICE as u8
            ),
            PeerMediaRole::Voice
        );
        assert_eq!(
            PeerMediaRole::from_stats_byte(
                mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_HOST as u8
            ),
            PeerMediaRole::StreamHost
        );
        assert_eq!(
            PeerMediaRole::from_stats_byte(
                mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER as u8
            ),
            PeerMediaRole::StreamViewer
        );
        assert_eq!(PeerMediaRole::from_stats_byte(255), PeerMediaRole::Voice);
    }

    #[test]
    fn classify_recv_result_paths() {
        assert_eq!(classify_recv_result(0).expect("empty"), RecvPoll::Empty);
        assert_eq!(
            classify_recv_result(VIDEO_RECV_ERROR).expect("error"),
            RecvPoll::Failed
        );
        assert_eq!(
            classify_recv_result(-128).expect("too small"),
            RecvPoll::TooSmall { required: 128 }
        );
        assert_eq!(
            classify_recv_result(42).expect("ready"),
            RecvPoll::Ready { bytes: 42 }
        );
    }

    #[test]
    fn recv_capacity_for_rejects_overflow() {
        let too_large = i32::try_from(MAX_ACCESS_UNIT_BYTES + 1).expect("fits in i32");
        assert_eq!(
            recv_capacity_for(too_large),
            Err(RtpPeerError::AccessUnitTooLarge {
                required: too_large
            })
        );
        assert_eq!(
            recv_capacity_for(MAX_ACCESS_UNIT_BYTES as i32),
            Ok(MAX_ACCESS_UNIT_BYTES)
        );
    }

    #[test]
    fn grow_recv_buffer_retains_capacity_on_repeat() {
        let mut buffer = vec![0_u8; 16];
        grow_recv_buffer(&mut buffer, 64).expect("grow");
        assert_eq!(buffer.len(), 64);
        let capacity = buffer.capacity();
        grow_recv_buffer(&mut buffer, 32).expect("no shrink");
        assert_eq!(buffer.len(), 64);
        assert!(buffer.capacity() >= capacity);
    }

    #[test]
    fn grow_recv_buffer_rejects_unbounded_native_size() {
        let mut buffer = Vec::new();
        let required = i32::try_from(MAX_ACCESS_UNIT_BYTES + 1).expect("fits in i32");
        assert_eq!(
            grow_recv_buffer(&mut buffer, required),
            Err(RtpPeerError::AccessUnitTooLarge { required })
        );
        assert!(buffer.is_empty());
    }
}
