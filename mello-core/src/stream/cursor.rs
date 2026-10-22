//! Stream pointer: cursor packets on the reliable control channel, and the
//! viewer's latest pointer for the presenter.
//!
//! Only the game capture hook sends them. Its frames are the game's back
//! buffer, which never has the system pointer, and drawing the pointer into
//! the frame would double the one a game draws itself. Screen capture has the
//! pointer in its frames (WGC) or draws it in (desktop duplication), and sends
//! one hidden packet when the stream moves to it. libmello decides
//! (`VideoPipeline::get_cursor_packet`); this module carries the bytes.
//!
//! Wire format (same family as the pause message `0x04/0x03`), the libmello
//! packet of `cursor.cpp` behind the control type byte:
//!
//! * `[0x04, 0x02, x: i32 LE, y: i32 LE, visible, has_shape]`
//! * with `has_shape`: `w: u16 LE, h: u16 LE`, then `w * h * 4` bytes RGBA
//!   (straight alpha).
//!
//! `x` and `y` are the top-left of the shape in the pixels of the encoded
//! picture; the shape is already scaled as the picture is.

use std::sync::{Arc, Mutex};

/// Control-message type shared with the pause message.
pub const CURSOR_MESSAGE_TYPE: u8 = 0x04;
/// Control-message subtype for the pointer (libmello `CURSOR_SUBTYPE`).
pub const CURSOR_MESSAGE_SUBTYPE: u8 = 0x02;
/// The largest shape side libmello sends (`kMaxCursorPacketSide`).
pub const MAX_CURSOR_SIDE: u16 = 128;
/// The largest libmello cursor packet, without the type byte.
pub const MAX_CURSOR_PACKET_BYTES: usize =
    15 + MAX_CURSOR_SIDE as usize * MAX_CURSOR_SIDE as usize * 4;

const HEADER: usize = 12;

/// A pointer bitmap, RGBA with straight alpha.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CursorShape {
    pub width: u16,
    pub height: u16,
    pub rgba: Vec<u8>,
}

/// One cursor message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorPacket {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    /// Present when the shape changed (and in the resend for a late joiner).
    pub shape: Option<CursorShape>,
}

/// Encode a cursor message for the control channel.
pub fn encode_cursor_message(packet: &CursorPacket) -> Vec<u8> {
    let shape_bytes = packet.shape.as_ref().map_or(0, |s| 4 + s.rgba.len());
    let mut out = Vec::with_capacity(HEADER + shape_bytes);
    out.push(CURSOR_MESSAGE_TYPE);
    out.push(CURSOR_MESSAGE_SUBTYPE);
    out.extend_from_slice(&packet.x.to_le_bytes());
    out.extend_from_slice(&packet.y.to_le_bytes());
    out.push(u8::from(packet.visible));
    out.push(u8::from(packet.shape.is_some()));
    if let Some(shape) = &packet.shape {
        out.extend_from_slice(&shape.width.to_le_bytes());
        out.extend_from_slice(&shape.height.to_le_bytes());
        out.extend_from_slice(&shape.rgba);
    }
    out
}

/// The control message for a packet from `mello_stream_get_cursor_packet`:
/// the type byte in front of libmello's bytes.
pub fn control_message_from_native(packet: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(packet.len() + 1);
    out.push(CURSOR_MESSAGE_TYPE);
    out.extend_from_slice(packet);
    out
}

/// Parse a control-channel datagram. `None` when it is another control
/// message, or a cursor message that is cut short (never a panic: the bytes
/// come from the network).
pub fn parse_cursor_message(data: &[u8]) -> Option<CursorPacket> {
    if data.len() < HEADER || data[0] != CURSOR_MESSAGE_TYPE || data[1] != CURSOR_MESSAGE_SUBTYPE {
        return None;
    }
    let x = i32::from_le_bytes(data[2..6].try_into().ok()?);
    let y = i32::from_le_bytes(data[6..10].try_into().ok()?);
    let visible = data[10] != 0;
    let shape = if data[11] != 0 {
        let size = data.get(HEADER..HEADER + 4)?;
        let width = u16::from_le_bytes([size[0], size[1]]);
        let height = u16::from_le_bytes([size[2], size[3]]);
        if width > MAX_CURSOR_SIDE || height > MAX_CURSOR_SIDE {
            return None;
        }
        let bytes = usize::from(width) * usize::from(height) * 4;
        let rgba = data.get(HEADER + 4..HEADER + 4 + bytes)?.to_vec();
        Some(CursorShape {
            width,
            height,
            rgba,
        })
    } else {
        None
    };
    Some(CursorPacket {
        x,
        y,
        visible,
        shape,
    })
}

/// True when a control message carries a shape (worth a resend to a viewer
/// that joins later: shapes are only sent when they change).
pub fn message_has_shape(data: &[u8]) -> bool {
    data.len() > HEADER
        && data[0] == CURSOR_MESSAGE_TYPE
        && data[1] == CURSOR_MESSAGE_SUBTYPE
        && data[11] != 0
}

/// The pointer a viewer draws over the stream.
#[derive(Debug, Clone, Default)]
pub struct ViewerCursor {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub shape: Option<Arc<CursorShape>>,
    /// Changes with every applied packet, so the presenter redraws only on a
    /// change. The shape is the same `Arc` until a new shape arrives.
    pub serial: u64,
}

/// The pointer of the stream being watched. One presenter shows one stream,
/// so there is one pointer: written by the control-channel callback (a
/// libdatachannel thread), read by the presenter tick (the UI thread).
static VIEWER_CURSOR: Mutex<ViewerCursor> = Mutex::new(ViewerCursor {
    x: 0,
    y: 0,
    visible: false,
    shape: None,
    serial: 0,
});

/// Apply one packet to `cursor`. A packet without a shape keeps the shape.
pub fn apply_cursor_packet(cursor: &mut ViewerCursor, packet: CursorPacket) {
    cursor.x = packet.x;
    cursor.y = packet.y;
    cursor.visible = packet.visible;
    if let Some(shape) = packet.shape {
        cursor.shape = Some(Arc::new(shape));
    }
    cursor.serial = cursor.serial.wrapping_add(1);
}

/// Apply a received packet to the watched stream's pointer.
pub fn apply_viewer_cursor(packet: CursorPacket) {
    if let Ok(mut cursor) = VIEWER_CURSOR.lock() {
        apply_cursor_packet(&mut cursor, packet);
    }
}

/// The watched stream's pointer, for the presenter.
pub fn viewer_cursor() -> ViewerCursor {
    VIEWER_CURSOR.lock().map(|c| c.clone()).unwrap_or_default()
}

/// Forget the pointer when the viewer stops watching, so the next stream does
/// not start with the last one's.
pub fn reset_viewer_cursor() {
    if let Ok(mut cursor) = VIEWER_CURSOR.lock() {
        let serial = cursor.serial.wrapping_add(1);
        *cursor = ViewerCursor {
            serial,
            ..ViewerCursor::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(width: u16, height: u16) -> CursorShape {
        CursorShape {
            width,
            height,
            rgba: (0..usize::from(width) * usize::from(height) * 4)
                .map(|i| (i % 251) as u8)
                .collect(),
        }
    }

    #[test]
    fn a_cursor_message_round_trips() {
        for packet in [
            CursorPacket {
                x: -3,
                y: 717,
                visible: true,
                shape: Some(shape(32, 32)),
            },
            CursorPacket {
                x: 1280,
                y: 0,
                visible: false,
                shape: None,
            },
            CursorPacket {
                x: i32::MIN,
                y: i32::MAX,
                visible: true,
                shape: Some(shape(MAX_CURSOR_SIDE, 1)),
            },
        ] {
            let bytes = encode_cursor_message(&packet);
            assert_eq!(parse_cursor_message(&bytes), Some(packet.clone()));
            assert_eq!(message_has_shape(&bytes), packet.shape.is_some());
        }
    }

    /// The bytes libmello writes (`serialize_cursor_packet`), with the type
    /// byte the host manager puts in front.
    #[test]
    fn a_native_packet_parses() {
        let mut native = vec![0x02];
        native.extend_from_slice(&100i32.to_le_bytes());
        native.extend_from_slice(&(-8i32).to_le_bytes());
        native.extend_from_slice(&[1, 1]);
        native.extend_from_slice(&2u16.to_le_bytes());
        native.extend_from_slice(&1u16.to_le_bytes());
        native.extend_from_slice(&[255, 0, 0, 255, 0, 0, 255, 128]);
        let message = control_message_from_native(&native);
        let packet = parse_cursor_message(&message).expect("a cursor message");
        assert_eq!((packet.x, packet.y, packet.visible), (100, -8, true));
        let s = packet.shape.expect("a shape");
        assert_eq!((s.width, s.height), (2, 1));
        assert_eq!(s.rgba, vec![255, 0, 0, 255, 0, 0, 255, 128]);
    }

    #[test]
    fn other_and_broken_messages_are_not_cursors() {
        assert_eq!(parse_cursor_message(&[0x04, 0x03, 1, 2]), None, "pause");
        assert_eq!(parse_cursor_message(&[0x04, 0x02, 1]), None, "short header");
        let mut cut = encode_cursor_message(&CursorPacket {
            x: 1,
            y: 2,
            visible: true,
            shape: Some(shape(4, 4)),
        });
        cut.truncate(cut.len() - 1);
        assert_eq!(parse_cursor_message(&cut), None, "short shape");
        let mut huge = encode_cursor_message(&CursorPacket {
            x: 1,
            y: 2,
            visible: true,
            shape: None,
        });
        huge[11] = 1;
        huge.extend_from_slice(&1000u16.to_le_bytes());
        huge.extend_from_slice(&1000u16.to_le_bytes());
        assert_eq!(
            parse_cursor_message(&huge),
            None,
            "a shape over the size cap"
        );
    }

    #[test]
    fn a_position_packet_keeps_the_shape() {
        let mut cursor = ViewerCursor::default();
        apply_cursor_packet(
            &mut cursor,
            CursorPacket {
                x: 1,
                y: 2,
                visible: true,
                shape: Some(shape(8, 8)),
            },
        );
        let first = cursor.shape.clone().expect("shape");
        let serial = cursor.serial;
        apply_cursor_packet(
            &mut cursor,
            CursorPacket {
                x: 5,
                y: 6,
                visible: true,
                shape: None,
            },
        );
        assert_eq!((cursor.x, cursor.y), (5, 6));
        assert!(Arc::ptr_eq(&first, cursor.shape.as_ref().expect("shape")));
        assert_ne!(cursor.serial, serial);
    }
}
