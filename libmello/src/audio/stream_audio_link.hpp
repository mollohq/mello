#pragma once

// The link between a stream host's game-audio pipeline and the host's SFU
// peer (streaming-quality stage 7, spec 12 section 9). The two objects meet
// only through mello-core: the pipeline gives Opus frames to Rust, and Rust
// sends them on the peer. The link carries what they must share inside
// libmello:
//
// - the capture time of each frame, by its media time. The peer puts it in
//   the A/V sync header extension of the frame's RTP packet;
// - the receiver reports about the audio send track. The pipeline sets the
//   encoder's expected loss (OPUS_SET_PACKET_LOSS_PERC) from them, as voice
//   does, so the in-band FEC follows the link.
//
// One link per MelloContext (one stream host per client). Threads: the
// capture thread records, the peer's send thread reads, the libdatachannel
// thread reports; one mutex guards each half.

#include "send_adaptation.hpp"

#include <array>
#include <atomic>
#include <cstdint>
#include <mutex>
#include <optional>

namespace mello::audio {

class StreamAudioLink {
public:
    /// Frames the capture-time ring holds: 5 s of 20 ms frames.
    static constexpr size_t kFrames = 256;
    /// Samples per frame (20 ms at 48 kHz).
    static constexpr uint32_t kFrameSamples = 960;

    StreamAudioLink() : adapt_(kSendMaxKbps) {}

    /// The host pipeline encoded the frame with media time `media_ts` (48 kHz
    /// samples) from audio captured at `capture_us` (steady clock, the clock
    /// of the video capture time). Capture thread.
    void record_capture(uint32_t media_ts, int64_t capture_us) {
        std::lock_guard<std::mutex> lock(capture_mutex_);
        Entry& e = ring_[slot(media_ts)];
        e.media_ts = media_ts;
        e.capture_us = capture_us;
        e.valid = true;
    }

    /// The capture time of the frame with media time `media_ts`, while it is
    /// in the ring. Send thread.
    std::optional<int64_t> capture_us(uint32_t media_ts) const {
        std::lock_guard<std::mutex> lock(capture_mutex_);
        const Entry& e = ring_[slot(media_ts)];
        if (!e.valid || e.media_ts != media_ts) return std::nullopt;
        return e.capture_us;
    }

    /// A new host stream: forget the capture times and the reports.
    void reset() {
        {
            std::lock_guard<std::mutex> lock(capture_mutex_);
            ring_ = {};
        }
        std::lock_guard<std::mutex> lock(adapt_mutex_);
        adapt_.reset();
        loss_perc_.store(adapt_.loss_perc(), std::memory_order_relaxed);
    }

    /// One receiver report about the audio send track (the fields of
    /// MelloSendFeedback). libdatachannel thread.
    void on_send_feedback(int64_t now_ms, uint32_t expected, uint32_t lost, float rtt_ms,
                          float jitter_ms) {
        std::lock_guard<std::mutex> lock(adapt_mutex_);
        adapt_.on_report(now_ms, expected, lost, rtt_ms, jitter_ms);
        loss_perc_.store(adapt_.loss_perc(), std::memory_order_relaxed);
        reports_.fetch_add(1, std::memory_order_relaxed);
    }

    /// The encoder's expected loss in percent: the voice rule (floor 5, cap
    /// 30, the loss over 4 s). Any thread.
    int loss_perc() const { return loss_perc_.load(std::memory_order_relaxed); }
    uint64_t reports() const { return reports_.load(std::memory_order_relaxed); }

private:
    struct Entry {
        uint32_t media_ts = 0;
        int64_t capture_us = 0;
        bool valid = false;
    };
    static size_t slot(uint32_t media_ts) { return (media_ts / kFrameSamples) % kFrames; }

    mutable std::mutex capture_mutex_;
    std::array<Entry, kFrames> ring_{};

    std::mutex adapt_mutex_;
    SendAdaptation adapt_;
    std::atomic<int> loss_perc_{kLossPercFloor};
    std::atomic<uint64_t> reports_{0};
};

} // namespace mello::audio
