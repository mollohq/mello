//! Safe wrappers over the libmello RTP peer video C ABI.
//!
//! Ownership of [`MelloPeerConnection`] stays with the caller: these helpers never
//! destroy peers. Every function documents the pointer validity it requires.

use std::ffi::CString;
use std::ptr::NonNull;

use mello_sys::{
    MelloContext, MelloPeerConnection, MelloPeerVideoFeedback, MelloRtpVideoAccessUnitInfo,
    MelloRtpVideoStats,
};
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
    pub(crate) fn to_ffi(self) -> mello_sys::MelloPeerMediaRole {
        match self {
            Self::Voice => mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_VOICE,
            Self::StreamHost => mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_HOST,
            Self::StreamViewer => mello_sys::MelloPeerMediaRole_MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER,
        }
    }

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

/// One host-side viewer feedback event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFeedback {
    Pli,
    Remb {
        bitrate_bps: u32,
    },
    LocalIdrNeeded,
    /// Send-side delay-gradient (GCC) estimate from TWCC feedback. The
    /// estimator smooths internally, so targets may be applied immediately.
    GccTarget {
        bitrate_bps: u32,
    },
}

/// Metadata for one received Annex-B H.264 access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedAccessUnit {
    pub is_idr: bool,
    pub rtp_timestamp: u32,
    pub capture_timestamp_us: u64,
}

/// Subset of native RTP video stats needed for stream orchestration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpVideoStats {
    pub media_role: PeerMediaRole,
    pub video_open: bool,
    pub tx_active: bool,
    pub rx_active: bool,
    pub tx_access_units_sent: u64,
    pub tx_access_units_dropped: u64,
    pub tx_bytes_sent: u64,
    pub rx_access_units_dropped: u64,
    pub tx_pacing_target_bps: u64,
    pub rx_receive_target_bps: u32,
    pub tx_latest_remb_bitrate_bps: u32,
    pub tx_rtx_sent: u64,
    pub tx_rtx_cache_misses: u64,
    pub tx_gcc_target_bps: u64,
    pub tx_fec_packets_sent: u64,
    pub rx_fec_recovered: u64,
    pub rx_fec_unrecoverable: u64,
}

/// Create a peer for `role`. Caller must eventually call `mello_peer_destroy`.
///
/// `ctx` must be a valid, live `MelloContext` pointer for the process lifetime
/// of the returned peer (libmello currently ignores it, but the contract is
/// retained for future context-scoped resources).
pub fn create_peer_for_role(
    ctx: NonNull<MelloContext>,
    peer_id: &str,
    role: PeerMediaRole,
) -> Result<NonNull<MelloPeerConnection>, RtpPeerError> {
    let peer_id_c = CString::new(peer_id).map_err(|_| RtpPeerError::InvalidPeerId)?;
    let peer = unsafe {
        mello_sys::mello_peer_create_for_role(ctx.as_ptr(), peer_id_c.as_ptr(), role.to_ffi())
    };
    NonNull::new(peer).ok_or(RtpPeerError::CreateFailed)
}

/// Send one Annex-B H.264 access unit on a stream-host peer.
///
/// `peer` must be a valid `MelloPeerConnection` pointer until this call returns.
pub fn send_access_unit(
    peer: NonNull<MelloPeerConnection>,
    annex_b: &[u8],
    capture_timestamp_us: u64,
) -> Result<(), RtpPeerError> {
    if annex_b.is_empty() {
        return Err(RtpPeerError::InvalidParam);
    }
    let result = unsafe {
        mello_sys::mello_peer_video_send_access_unit(
            peer.as_ptr(),
            annex_b.as_ptr(),
            i32::try_from(annex_b.len()).map_err(|_| RtpPeerError::InvalidParam)?,
            capture_timestamp_us,
        )
    };
    map_mello_result(result)
}

/// Send one Opus frame on a stream-host peer.
pub fn send_audio(peer: NonNull<MelloPeerConnection>, opus: &[u8]) -> Result<(), RtpPeerError> {
    if opus.is_empty() {
        return Ok(());
    }
    let result = unsafe {
        mello_sys::mello_peer_send_audio(
            peer.as_ptr(),
            opus.as_ptr(),
            i32::try_from(opus.len()).map_err(|_| RtpPeerError::InvalidParam)?,
        )
    };
    map_mello_result(result)
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

/// Poll one queued host-side feedback event, if any.
///
/// `peer` must be a valid `MelloPeerConnection` pointer until this call returns.
pub fn poll_video_feedback(
    peer: NonNull<MelloPeerConnection>,
) -> Result<Option<VideoFeedback>, RtpPeerError> {
    let mut feedback = MelloPeerVideoFeedback {
        type_: mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_PLI,
        remb_bitrate_bps: 0,
    };
    let has_feedback =
        unsafe { mello_sys::mello_peer_video_take_feedback(peer.as_ptr(), &mut feedback) != 0 };
    if !has_feedback {
        return Ok(None);
    }
    Ok(Some(video_feedback_from_ffi(&feedback)))
}

/// Set the stream-host RTP pacing target in bits per second.
pub fn set_pacing_target(peer: NonNull<MelloPeerConnection>, bps: u64) -> Result<(), RtpPeerError> {
    if bps == 0 {
        return Err(RtpPeerError::InvalidParam);
    }
    let result = unsafe { mello_sys::mello_peer_video_set_pacing_target(peer.as_ptr(), bps) };
    map_mello_result(result)
}

/// Set the stream-viewer receive target in bits per second.
pub fn set_receive_target(
    peer: NonNull<MelloPeerConnection>,
    bps: u32,
) -> Result<(), RtpPeerError> {
    if bps == 0 {
        return Err(RtpPeerError::InvalidParam);
    }
    let result = unsafe { mello_sys::mello_peer_video_set_receive_target(peer.as_ptr(), bps) };
    map_mello_result(result)
}

/// Returns whether the native RTP video sender or receiver track is open.
pub fn video_is_open(peer: NonNull<MelloPeerConnection>) -> bool {
    unsafe { mello_sys::mello_peer_video_is_open(peer.as_ptr()) != 0 }
}

/// Snapshot native RTP video stats into a typed, orchestration-focused view.
pub fn snapshot_video_stats(peer: NonNull<MelloPeerConnection>) -> RtpVideoStats {
    let mut stats = zero_video_stats();
    unsafe { mello_sys::mello_peer_video_get_stats(peer.as_ptr(), &mut stats) };
    stats_from_native(&stats)
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

fn video_feedback_from_ffi(feedback: &MelloPeerVideoFeedback) -> VideoFeedback {
    match feedback.type_ {
        mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_REMB => {
            VideoFeedback::Remb {
                bitrate_bps: feedback.remb_bitrate_bps,
            }
        }
        mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_LOCAL_IDR_NEEDED => {
            VideoFeedback::LocalIdrNeeded
        }
        mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_GCC_TARGET => {
            VideoFeedback::GccTarget {
                bitrate_bps: feedback.remb_bitrate_bps,
            }
        }
        _ => VideoFeedback::Pli,
    }
}

fn stats_from_native(stats: &MelloRtpVideoStats) -> RtpVideoStats {
    RtpVideoStats {
        media_role: PeerMediaRole::from_stats_byte(stats.media_role),
        video_open: stats.video_open != 0,
        tx_active: stats.tx_active != 0,
        rx_active: stats.rx_active != 0,
        tx_access_units_sent: stats.tx_access_units_sent,
        tx_access_units_dropped: stats.tx_access_units_dropped,
        tx_bytes_sent: stats.tx_bytes_sent,
        rx_access_units_dropped: stats.rx_access_units_dropped,
        tx_pacing_target_bps: stats.tx_pacing_target_bps,
        rx_receive_target_bps: stats.rx_receive_target_bps,
        tx_latest_remb_bitrate_bps: stats.tx_latest_remb_bitrate_bps,
        tx_rtx_sent: stats.tx_rtx_sent,
        tx_rtx_cache_misses: stats.tx_rtx_cache_misses,
        tx_gcc_target_bps: stats.tx_gcc_target_bps,
        tx_fec_packets_sent: stats.tx_fec_packets_sent,
        rx_fec_recovered: stats.rx_fec_recovered,
        rx_fec_unrecoverable: stats.rx_fec_unrecoverable,
    }
}

fn map_mello_result(result: mello_sys::MelloResult) -> Result<(), RtpPeerError> {
    match result {
        mello_sys::MelloResult_MELLO_OK => Ok(()),
        mello_sys::MelloResult_MELLO_ERROR_INVALID_PARAM => Err(RtpPeerError::InvalidParam),
        mello_sys::MelloResult_MELLO_ERROR_TRANSPORT_BACKPRESSURE => {
            Err(RtpPeerError::Backpressure)
        }
        _ => Err(RtpPeerError::TransportFailed),
    }
}

fn zero_access_unit_info() -> MelloRtpVideoAccessUnitInfo {
    MelloRtpVideoAccessUnitInfo {
        size: 0,
        is_idr: 0,
        rtp_timestamp: 0,
        capture_timestamp_us: 0,
    }
}

fn zero_video_stats() -> MelloRtpVideoStats {
    // SAFETY: mello_peer_video_get_stats fully overwrites the struct.
    unsafe { std::mem::zeroed() }
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

    #[test]
    fn video_feedback_from_ffi_maps_all_variants() {
        assert_eq!(
            video_feedback_from_ffi(&MelloPeerVideoFeedback {
                type_: mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_PLI,
                remb_bitrate_bps: 0,
            }),
            VideoFeedback::Pli
        );
        assert_eq!(
            video_feedback_from_ffi(&MelloPeerVideoFeedback {
                type_: mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_REMB,
                remb_bitrate_bps: 4_500_000,
            }),
            VideoFeedback::Remb {
                bitrate_bps: 4_500_000
            }
        );
        assert_eq!(
            video_feedback_from_ffi(&MelloPeerVideoFeedback {
                type_:
                    mello_sys::MelloPeerVideoFeedbackType_MELLO_PEER_VIDEO_FEEDBACK_LOCAL_IDR_NEEDED,
                remb_bitrate_bps: 0,
            }),
            VideoFeedback::LocalIdrNeeded
        );
    }

    #[test]
    fn map_mello_result_maps_invalid_param() {
        assert_eq!(
            map_mello_result(mello_sys::MelloResult_MELLO_ERROR_INVALID_PARAM),
            Err(RtpPeerError::InvalidParam)
        );
        assert_eq!(
            map_mello_result(mello_sys::MelloResult_MELLO_ERROR_TRANSPORT_FAILED),
            Err(RtpPeerError::TransportFailed)
        );
        assert_eq!(
            map_mello_result(mello_sys::MelloResult_MELLO_ERROR_TRANSPORT_BACKPRESSURE),
            Err(RtpPeerError::Backpressure)
        );
        assert_eq!(map_mello_result(mello_sys::MelloResult_MELLO_OK), Ok(()));
    }
}
