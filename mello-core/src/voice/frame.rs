//! The voice frame of the mic test (spec 10 section 4.2).
//!
//! ```text
//! [sequence: u32 LE][rtp timestamp: u32 LE][opus payload ...]
//! ```
//!
//! `mello_voice_feed_packet` takes this frame. libmello builds the RTP fields
//! for NetEQ from it (payload type 111, the low 16 bits of the sequence, the
//! timestamp), so the loopback path plays through the same NetEQ as the SFU
//! path. `mello_voice_get_packet` still returns only the 4-byte
//! sequence header; the timestamp comes from
//! `mello_voice_get_packet_with_timestamp`.

/// Bytes before the Opus payload.
pub const HEADER_LEN: usize = 8;

/// Bytes of the sequence header that `mello_voice_get_packet` puts before
/// the Opus payload.
pub const GET_PACKET_HEADER_LEN: usize = 4;

/// Write the frame for `sequence`, `timestamp` and `opus` into `out`.
/// Returns the frame length, or None when `out` is too small.
pub fn write(sequence: u32, timestamp: u32, opus: &[u8], out: &mut [u8]) -> Option<usize> {
    let len = HEADER_LEN + opus.len();
    let dst = out.get_mut(..len)?;
    dst[..4].copy_from_slice(&sequence.to_le_bytes());
    dst[4..HEADER_LEN].copy_from_slice(&timestamp.to_le_bytes());
    dst[HEADER_LEN..].copy_from_slice(opus);
    Some(len)
}

/// The frame for a packet from `mello_voice_get_packet_with_timestamp`: its
/// 4-byte sequence header and Opus payload, and its timestamp. None for a
/// packet with no payload.
pub fn from_get_packet(packet: &[u8], timestamp: u32) -> Option<Vec<u8>> {
    if packet.len() <= GET_PACKET_HEADER_LEN {
        return None;
    }
    let mut frame = vec![0u8; HEADER_LEN + packet.len() - GET_PACKET_HEADER_LEN];
    frame[..4].copy_from_slice(&packet[..GET_PACKET_HEADER_LEN]);
    frame[4..HEADER_LEN].copy_from_slice(&timestamp.to_le_bytes());
    frame[HEADER_LEN..].copy_from_slice(&packet[GET_PACKET_HEADER_LEN..]);
    Some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_sequence_then_timestamp_then_opus() {
        let mut buf = [0u8; 16];
        let n = write(0x0403_0201, 0x0807_0605, &[0xAA, 0xBB], &mut buf).expect("fits");
        assert_eq!(n, 10);
        assert_eq!(&buf[..n], &[1, 2, 3, 4, 5, 6, 7, 8, 0xAA, 0xBB]);
        assert!(
            write(1, 2, &[0; 9], &mut buf).is_none(),
            "16 bytes cannot hold 8 + 9"
        );
    }

    #[test]
    fn get_packet_header_carries_over_with_the_timestamp() {
        let packet = [9, 0, 0, 0, 0xCC, 0xDD];
        let frame = from_get_packet(&packet, 960 * 3).expect("has payload");
        assert_eq!(frame, vec![9, 0, 0, 0, 0x40, 0x0B, 0, 0, 0xCC, 0xDD]);
        assert!(from_get_packet(&packet[..4], 0).is_none());
    }
}
