const PACER_MIN_TARGET_KBPS: u32 = 250;

#[derive(Debug, Clone, Copy, Default)]
pub struct PacingTelemetry {
    pub target_kbps: u32,
    pub paced_bytes: u64,
    pub sleep_count: u64,
    pub sleep_ms_total: u64,
}

/// Headroom for RTP/UDP/IP headers above the encoded H.264 bitrate.
const RTP_PACING_OVERHEAD_FACTOR: f64 = 1.08;

pub(crate) fn calc_stream_pacing_target_kbps(video_bitrate_kbps: u32) -> u32 {
    (video_bitrate_kbps as f64 * RTP_PACING_OVERHEAD_FACTOR)
        .round()
        .max(PACER_MIN_TARGET_KBPS as f64) as u32
}

#[cfg(test)]
mod tests {
    use super::calc_stream_pacing_target_kbps;

    #[test]
    fn pacing_target_adds_rtp_header_headroom() {
        assert_eq!(calc_stream_pacing_target_kbps(4_000), 4_320);
        assert_eq!(calc_stream_pacing_target_kbps(0), 250);
    }
}
