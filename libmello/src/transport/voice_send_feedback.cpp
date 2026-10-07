#include "voice_send_feedback.hpp"

#include <chrono>
#include <cmath>

namespace mello::transport {
namespace {

constexpr uint8_t kRtcpSenderReport = 200;
constexpr uint8_t kRtcpReceiverReport = 201;
constexpr size_t kRtcpHeaderSize = 4;
constexpr size_t kReportBlockSize = 24;
// Sender SSRC (4) + NTP (8) + RTP timestamp (4) + packet and octet count (8).
constexpr size_t kSenderInfoSize = 24;
// A report interval longer than this many packets is a reset, not traffic
// (10 minutes of 20 ms frames).
constexpr uint32_t kMaxIntervalPackets = 30000;
// A round trip longer than this is a clock step or a stale LSR.
constexpr double kMaxRttMs = 60000.0;

uint32_t read_be32(const uint8_t* p) {
    return (static_cast<uint32_t>(p[0]) << 24) | (static_cast<uint32_t>(p[1]) << 16) |
           (static_cast<uint32_t>(p[2]) << 8) | static_cast<uint32_t>(p[3]);
}

ReceptionReport read_block(const uint8_t* p, uint32_t reporter) {
    ReceptionReport r;
    r.reporter_ssrc = reporter;
    r.source_ssrc = read_be32(p);
    const uint32_t lost = read_be32(p + 4);
    r.fraction_lost = static_cast<uint8_t>(lost >> 24);
    // Signed 24-bit: sign-extend bit 23.
    uint32_t cumulative = lost & 0x00FFFFFFu;
    if (cumulative & 0x00800000u) {
        cumulative |= 0xFF000000u;
    }
    r.cumulative_lost = static_cast<int32_t>(cumulative);
    r.extended_highest_seq = read_be32(p + 8);
    r.jitter = read_be32(p + 12);
    r.last_sr = read_be32(p + 16);
    r.delay_since_last_sr = read_be32(p + 20);
    return r;
}

} // namespace

std::vector<ReceptionReport> find_reception_reports(
    const uint8_t* data,
    size_t size,
    uint32_t source_ssrc
) {
    std::vector<ReceptionReport> out;
    if (!data) {
        return out;
    }
    size_t offset = 0;
    while (offset + kRtcpHeaderSize <= size) {
        const uint8_t* p = data + offset;
        if ((p[0] >> 6) != 2) {
            break;
        }
        const size_t count = p[0] & 0x1F;
        const uint8_t type = p[1];
        const size_t length = (static_cast<size_t>((p[2] << 8) | p[3]) + 1) * 4;
        if (offset + length > size) {
            break;
        }
        size_t blocks_at = 0;
        if (type == kRtcpReceiverReport) {
            blocks_at = kRtcpHeaderSize + 4;
        } else if (type == kRtcpSenderReport) {
            blocks_at = kRtcpHeaderSize + kSenderInfoSize;
        }
        if (blocks_at != 0) {
            if (blocks_at + count * kReportBlockSize > length) {
                break;
            }
            const uint32_t reporter = read_be32(p + 4);
            for (size_t i = 0; i < count; ++i) {
                const uint8_t* block = p + blocks_at + i * kReportBlockSize;
                if (read_be32(block) == source_ssrc) {
                    out.push_back(read_block(block, reporter));
                }
            }
        }
        offset += length;
    }
    return out;
}

uint32_t ntp_compact_now() {
    // Same clock and epoch as rtc::RtcpSrReporter (libdatachannel
    // rtcpsrreporter.cpp): system clock, NTP epoch 1900.
    const auto now = std::chrono::system_clock::now();
    const double secs = std::chrono::duration<double>(now.time_since_epoch()).count();
    const auto ntp = static_cast<uint64_t>(
        std::floor((secs + 2208988800.0) * static_cast<double>(uint64_t(1) << 32)));
    return static_cast<uint32_t>(ntp >> 16);
}

float rtt_ms_from_report(uint32_t last_sr, uint32_t delay_since_last_sr,
                         uint32_t arrival_ntp_compact) {
    if (last_sr == 0) {
        return -1.0f;
    }
    // Modulo 2^32: the compact NTP time wraps every 18 hours.
    const uint32_t since_sr = arrival_ntp_compact - last_sr;
    if (since_sr < delay_since_last_sr) {
        return -1.0f;
    }
    const double rtt_ms = static_cast<double>(since_sr - delay_since_last_sr) * 1000.0 / 65536.0;
    if (rtt_ms > kMaxRttMs) {
        return -1.0f;
    }
    return static_cast<float>(rtt_ms);
}

void SendFeedbackTracker::set_first_sequence(uint16_t sequence) {
    std::lock_guard<std::mutex> lock(mutex_);
    have_first_sequence_ = true;
    first_sequence_ = sequence;
    have_previous_ = false;
}

MelloSendFeedback SendFeedbackTracker::on_report(const ReceptionReport& report,
                                                 uint32_t arrival_ntp_compact,
                                                 uint32_t clock_rate) {
    std::lock_guard<std::mutex> lock(mutex_);
    MelloSendFeedback fb{};
    fb.reports = ++reports_;
    fb.fraction_lost = static_cast<float>(report.fraction_lost) / 256.0f;
    fb.cumulative_lost = report.cumulative_lost;
    fb.extended_highest_seq = report.extended_highest_seq;
    fb.jitter_ms = clock_rate > 0
                       ? static_cast<float>(report.jitter) * 1000.0f / static_cast<float>(clock_rate)
                       : 0.0f;
    fb.rtt_ms = rtt_ms_from_report(report.last_sr, report.delay_since_last_sr,
                                   arrival_ntp_compact);
    fb.rtt_fresh = fb.rtt_ms >= 0.0f && report.last_sr != previous_last_sr_;
    previous_last_sr_ = report.last_sr;

    // Packets expected since the previous report (RFC 3550 appendix A.3).
    // The first report of a track counts from the track's first sequence;
    // the reporter's cycle count starts at its first packet, so compare 16
    // bits only.
    uint32_t expected = 0;
    int64_t lost_delta = 0;
    // Pion reports a stream from the time it binds it, also before the first
    // packet: sequence 0, nothing lost. Such a report says nothing, and it
    // must not become the previous report of the first real one.
    const bool before_first_packet = !have_previous_ && report.extended_highest_seq == 0 &&
                                     report.cumulative_lost == 0 && report.fraction_lost == 0;
    if (before_first_packet) {
        // No interval.
    } else if (have_previous_) {
        const uint32_t delta = report.extended_highest_seq - previous_extended_seq_;
        expected = delta <= kMaxIntervalPackets ? delta : 0;
        lost_delta = static_cast<int64_t>(report.cumulative_lost) - previous_cumulative_lost_;
    } else if (have_first_sequence_) {
        const auto delta = static_cast<uint16_t>(
            static_cast<uint16_t>(report.extended_highest_seq) - first_sequence_);
        expected = delta < 0x8000u ? static_cast<uint32_t>(delta) + 1 : 0;
        lost_delta = report.cumulative_lost;
    }
    uint32_t lost = 0;
    if (expected > 0) {
        if (lost_delta >= 0 && lost_delta <= static_cast<int64_t>(expected)) {
            lost = static_cast<uint32_t>(lost_delta);
        } else {
            // Duplicates or a reporter reset make the difference unusable.
            // The fraction covers the same interval.
            lost = static_cast<uint32_t>(
                std::lround(static_cast<double>(report.fraction_lost) * expected / 256.0));
        }
    }
    fb.interval_expected = expected;
    fb.interval_lost = lost;

    have_previous_ = have_previous_ || !before_first_packet;
    previous_extended_seq_ = report.extended_highest_seq;
    previous_cumulative_lost_ = report.cumulative_lost;
    latest_ = fb;
    have_latest_ = true;
    return fb;
}

bool SendFeedbackTracker::latest(MelloSendFeedback* out) const {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!have_latest_ || !out) {
        return false;
    }
    *out = latest_;
    return true;
}

} // namespace mello::transport
