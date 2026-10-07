#pragma once

#include "mello.h"

#include <cstddef>
#include <cstdint>
#include <mutex>
#include <vector>

namespace mello::transport {

// Receiver reports for the voice send track (spec 10 section 7.5).
//
// The SFU (Pion, report.NewReceiverInterceptor) sends one RTCP receiver
// report per second for the client's microphone track. Each report block
// carries the loss and the jitter that the SFU measured on our send leg, and
// the LSR and DLSR fields that give the round trip against the sender
// reports that rtc::RtcpSrReporter sends on the same track.

/// One RTCP reception report block about one source (RFC 3550 section
/// 6.4.1), as on the wire.
struct ReceptionReport {
    /// SSRC of the reporter (the RR or SR sender).
    uint32_t reporter_ssrc = 0;
    /// SSRC of the source that the block is about.
    uint32_t source_ssrc = 0;
    /// Fraction of packets lost since the previous report, in 1/256.
    uint8_t fraction_lost = 0;
    /// Cumulative number of packets lost, signed 24-bit.
    int32_t cumulative_lost = 0;
    /// Extended highest sequence number received (cycles << 16 | seq).
    uint32_t extended_highest_seq = 0;
    /// Interarrival jitter in RTP timestamp units.
    uint32_t jitter = 0;
    /// LSR: the middle 32 bits of the NTP time of the last sender report.
    uint32_t last_sr = 0;
    /// DLSR: time from that sender report to this report, in 1/65536 s.
    uint32_t delay_since_last_sr = 0;
};

/// Find the report blocks about `source_ssrc` in one RTCP packet. A
/// compound packet is fine: receiver reports (PT 201) and sender reports
/// (PT 200) carry report blocks, other packet types are skipped. The walk
/// stops at the first malformed header. Returns the blocks in packet order.
std::vector<ReceptionReport> find_reception_reports(
    const uint8_t* data,
    size_t size,
    uint32_t source_ssrc
);

/// The middle 32 bits of the NTP time now, on the clock that
/// rtc::RtcpSrReporter writes into its sender reports (the system clock).
uint32_t ntp_compact_now();

/// Round trip in ms from one report block that arrived at
/// `arrival_ntp_compact` (RFC 3550 section 6.4.1: A - LSR - DLSR, in
/// 1/65536 s). Returns -1 when the reporter has seen no sender report
/// (LSR = 0), or when the result is negative or longer than 60 s (a clock
/// step or a report about an old sender report).
float rtt_ms_from_report(uint32_t last_sr, uint32_t delay_since_last_sr,
                         uint32_t arrival_ntp_compact);

/// Turns the reports about the send track into MelloSendFeedback: the
/// last report's fields, the round trip, and the packets expected and lost
/// since the previous report (RFC 3550 appendix A.3). Thread safe.
class SendFeedbackTracker {
public:
    /// A new send track starts at RTP sequence `sequence` (a new
    /// packetizer). The next report's interval starts there; the previous
    /// track's sequence and loss counts are forgotten. Without a call, the
    /// first report gives no interval (expected 0). The report count stays.
    void set_first_sequence(uint16_t sequence);

    /// Take one report block that arrived at `arrival_ntp_compact`. Jitter
    /// converts with `clock_rate` (48000 for Opus). Returns the feedback for
    /// this report.
    MelloSendFeedback on_report(const ReceptionReport& report,
                                uint32_t arrival_ntp_compact,
                                uint32_t clock_rate);

    /// The feedback of the last report. False when no report arrived yet.
    bool latest(MelloSendFeedback* out) const;

private:
    mutable std::mutex mutex_;
    bool have_first_sequence_ = false;
    uint16_t first_sequence_ = 0;
    bool have_previous_ = false;
    uint32_t previous_extended_seq_ = 0;
    int32_t previous_cumulative_lost_ = 0;
    // LSR of the previous report, for rtt_fresh.
    uint32_t previous_last_sr_ = 0;
    uint32_t reports_ = 0;
    bool have_latest_ = false;
    MelloSendFeedback latest_{};
};

} // namespace mello::transport
