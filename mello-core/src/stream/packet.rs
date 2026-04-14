use std::time::{SystemTime, UNIX_EPOCH};

pub const HEADER_SIZE: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Video = 0x01,
    Audio = 0x02,
    Fec = 0x03,
    Control = 0x04,
}

impl PacketType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::Video),
            0x02 => Some(Self::Audio),
            0x03 => Some(Self::Fec),
            0x04 => Some(Self::Control),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlSubtype {
    LossReport = 0x01,
    KeyframeRequest = 0x02,
    QualityChange = 0x03,
}

impl ControlSubtype {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::LossReport),
            0x02 => Some(Self::KeyframeRequest),
            0x03 => Some(Self::QualityChange),
            _ => None,
        }
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PacketFlags: u8 {
        const IS_KEYFRAME     = 0b0000_0001;
        const FEC_GROUP_LAST  = 0b0000_0010;
        const CODEC_AV1       = 0b0000_0100;
    }
}

#[derive(Debug, Clone)]
pub struct StreamPacket {
    pub ptype: PacketType,
    pub flags: PacketFlags,
    pub sequence: u16,
    pub timestamp_us: u64,
    pub payload: Vec<u8>,
}

impl StreamPacket {
    pub fn new(ptype: PacketType, flags: PacketFlags, sequence: u16, payload: Vec<u8>) -> Self {
        let timestamp_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        Self {
            ptype,
            flags,
            sequence,
            timestamp_us,
            payload,
        }
    }

    pub fn video(payload: Vec<u8>, sequence: u16, is_keyframe: bool, fec_group_last: bool) -> Self {
        let mut flags = PacketFlags::empty();
        if is_keyframe {
            flags |= PacketFlags::IS_KEYFRAME;
        }
        if fec_group_last {
            flags |= PacketFlags::FEC_GROUP_LAST;
        }
        Self::new(PacketType::Video, flags, sequence, payload)
    }

    pub fn audio(payload: Vec<u8>, sequence: u16) -> Self {
        Self::new(PacketType::Audio, PacketFlags::empty(), sequence, payload)
    }

    pub fn fec(parity_payload: Vec<u8>, sequence: u16) -> Self {
        Self::new(
            PacketType::Fec,
            PacketFlags::empty(),
            sequence,
            parity_payload,
        )
    }

    pub fn control(payload: Vec<u8>, sequence: u16) -> Self {
        Self::new(PacketType::Control, PacketFlags::empty(), sequence, payload)
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags.contains(PacketFlags::IS_KEYFRAME)
    }

    pub fn is_fec_group_last(&self) -> bool {
        self.flags.contains(PacketFlags::FEC_GROUP_LAST)
    }

    /// Serialize to wire format: 12-byte header + payload.
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_SIZE + self.payload.len());
        buf.push(self.ptype as u8);
        buf.push(self.flags.bits());
        buf.extend_from_slice(&self.sequence.to_be_bytes());
        buf.extend_from_slice(&self.timestamp_us.to_be_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Parse from wire format. Returns None if buffer is too short or type is invalid.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < HEADER_SIZE {
            return None;
        }
        let ptype = PacketType::from_u8(data[0])?;
        let flags = PacketFlags::from_bits_truncate(data[1]);
        let sequence = u16::from_be_bytes([data[2], data[3]]);
        let timestamp_us = u64::from_be_bytes(data[4..12].try_into().ok()?);
        let payload = data[HEADER_SIZE..].to_vec();
        Some(Self {
            ptype,
            flags,
            sequence,
            timestamp_us,
            payload,
        })
    }
}

/// Viewer -> Host loss report, sent inside a Control packet payload.
#[derive(Debug, Clone, Copy)]
pub struct LossReport {
    pub packets_received: u16,
    pub packets_lost: u16,
    /// Optional app-level observed receive throughput for ABR v2 (kbps).
    pub observed_rx_kbps: Option<u16>,
}

impl LossReport {
    pub const WIRE_SIZE_V1: usize = 6; // subtype(1) + received(2) + lost(2) + reserved(1)
    pub const WIRE_SIZE_V2: usize = 8; // v1 + observed_rx_kbps(2)

    pub fn serialize(&self) -> Vec<u8> {
        let v2 = self.observed_rx_kbps.is_some();
        let mut buf = Vec::with_capacity(if v2 {
            Self::WIRE_SIZE_V2
        } else {
            Self::WIRE_SIZE_V1
        });
        buf.push(ControlSubtype::LossReport as u8);
        buf.extend_from_slice(&self.packets_received.to_be_bytes());
        buf.extend_from_slice(&self.packets_lost.to_be_bytes());
        buf.push(0); // reserved
        if let Some(kbps) = self.observed_rx_kbps {
            buf.extend_from_slice(&kbps.to_be_bytes());
        }
        buf
    }

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < Self::WIRE_SIZE_V1 {
            return None;
        }
        if data[0] != ControlSubtype::LossReport as u8 {
            return None;
        }
        let packets_received = u16::from_be_bytes([data[1], data[2]]);
        let packets_lost = u16::from_be_bytes([data[3], data[4]]);
        let observed_rx_kbps = if data.len() >= Self::WIRE_SIZE_V2 {
            let kbps = u16::from_be_bytes([data[6], data[7]]);
            if kbps == 0 {
                None
            } else {
                Some(kbps)
            }
        } else {
            None
        };
        Some(Self {
            packets_received,
            packets_lost,
            observed_rx_kbps,
        })
    }

    pub fn loss_ratio(&self) -> f32 {
        let total = self.packets_received as f32 + self.packets_lost as f32;
        if total == 0.0 {
            return 0.0;
        }
        self.packets_lost as f32 / total
    }
}

/// Keyframe request control payload.
pub struct KeyframeRequest;

impl KeyframeRequest {
    pub fn serialize() -> Vec<u8> {
        vec![ControlSubtype::KeyframeRequest as u8]
    }

    pub fn parse(data: &[u8]) -> bool {
        !data.is_empty() && data[0] == ControlSubtype::KeyframeRequest as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_video_packet() {
        let pkt = StreamPacket::video(vec![1, 2, 3, 4], 42, true, false);
        let bytes = pkt.serialize();
        assert_eq!(bytes.len(), HEADER_SIZE + 4);

        let parsed = StreamPacket::parse(&bytes).unwrap();
        assert_eq!(parsed.ptype, PacketType::Video);
        assert!(parsed.is_keyframe());
        assert!(!parsed.is_fec_group_last());
        assert_eq!(parsed.sequence, 42);
        assert_eq!(parsed.payload, vec![1, 2, 3, 4]);
    }

    #[test]
    fn roundtrip_audio_packet() {
        let pkt = StreamPacket::audio(vec![0xAA; 160], 7);
        let bytes = pkt.serialize();
        let parsed = StreamPacket::parse(&bytes).unwrap();
        assert_eq!(parsed.ptype, PacketType::Audio);
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.payload.len(), 160);
    }

    #[test]
    fn roundtrip_loss_report() {
        let report = LossReport {
            packets_received: 950,
            packets_lost: 50,
            observed_rx_kbps: Some(7_500),
        };
        let bytes = report.serialize();
        assert_eq!(bytes.len(), LossReport::WIRE_SIZE_V2);

        let parsed = LossReport::parse(&bytes).unwrap();
        assert_eq!(parsed.packets_received, 950);
        assert_eq!(parsed.packets_lost, 50);
        assert_eq!(parsed.observed_rx_kbps, Some(7_500));
        assert!((parsed.loss_ratio() - 0.05).abs() < 0.001);
    }

    #[test]
    fn parse_legacy_loss_report_v1() {
        let mut v1 = Vec::with_capacity(LossReport::WIRE_SIZE_V1);
        v1.push(ControlSubtype::LossReport as u8);
        v1.extend_from_slice(&100u16.to_be_bytes());
        v1.extend_from_slice(&5u16.to_be_bytes());
        v1.push(0);

        let parsed = LossReport::parse(&v1).unwrap();
        assert_eq!(parsed.packets_received, 100);
        assert_eq!(parsed.packets_lost, 5);
        assert_eq!(parsed.observed_rx_kbps, None);
    }

    #[test]
    fn serialize_loss_report_v1_when_no_throughput() {
        let report = LossReport {
            packets_received: 123,
            packets_lost: 7,
            observed_rx_kbps: None,
        };
        let bytes = report.serialize();
        assert_eq!(bytes.len(), LossReport::WIRE_SIZE_V1);

        let parsed = LossReport::parse(&bytes).unwrap();
        assert_eq!(parsed.packets_received, 123);
        assert_eq!(parsed.packets_lost, 7);
        assert_eq!(parsed.observed_rx_kbps, None);
    }

    #[test]
    fn parse_loss_report_v2_zero_throughput_treated_as_none() {
        let mut v2 = Vec::with_capacity(LossReport::WIRE_SIZE_V2);
        v2.push(ControlSubtype::LossReport as u8);
        v2.extend_from_slice(&100u16.to_be_bytes());
        v2.extend_from_slice(&2u16.to_be_bytes());
        v2.push(0);
        v2.extend_from_slice(&0u16.to_be_bytes());

        let parsed = LossReport::parse(&v2).unwrap();
        assert_eq!(parsed.packets_received, 100);
        assert_eq!(parsed.packets_lost, 2);
        assert_eq!(parsed.observed_rx_kbps, None);
    }

    #[test]
    fn parse_too_short() {
        assert!(StreamPacket::parse(&[0u8; 4]).is_none());
    }

    #[test]
    fn parse_invalid_type() {
        let mut data = [0u8; HEADER_SIZE];
        data[0] = 0xFF;
        assert!(StreamPacket::parse(&data).is_none());
    }

    #[test]
    fn keyframe_request_roundtrip() {
        let bytes = KeyframeRequest::serialize();
        assert!(KeyframeRequest::parse(&bytes));
        assert!(!KeyframeRequest::parse(&[]));
    }
}
