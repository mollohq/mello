#pragma once

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <memory>
#include <mutex>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

namespace mello::transport {

// Voice receive slots (spec 10 section 7.1, mello-sfu 01-SFU.md section 5.4).
// The SFU names the member that each receive m-line carries, by mid, in
// `slot` control messages. The track callback resolves the sender of each
// RTP packet here, so no SDP attribute (msid) names the sender.
//
// RTP and the control channel are separate transports: the first packets of
// a new owner can arrive before its `slot` message (stage 4b soak: about
// 0.009 % of slot packets, up to 116 ms early). route() holds a packet on a
// mid with no owner for kHoldMs; bind() returns the held packets so the
// caller plays them. A held packet older than kHoldMs is dropped and counted.
//
// A released mid can also get late packets of its old owner (sent before the
// release, delivered after it). They continue the old owner's RTP timestamps;
// the next owner's packets do not. route() drops a packet whose timestamp is
// within kLateWindowSamples of the last packet routed on the mid before the
// release, so a late packet never reaches the next owner.
//
// Thread safe. bind() runs on mello-core's control-channel callback thread,
// route() on the libdatachannel track thread for every packet.
class VoiceSlotMap {
public:
    // How long a packet on a mid with no owner waits for its `slot` message.
    static constexpr int64_t kHoldMs = 200;
    // Most packets held per mid: 200 ms of 10 ms frames, with room.
    static constexpr size_t kMaxHeldPackets = 32;
    // A packet within 1 s (48 kHz) of the released owner's last timestamp is
    // a late packet of that owner.
    static constexpr int32_t kLateWindowSamples = 48'000;

    struct Bound {
        // The previous sender of the mid, "" if it had none.
        std::string previous;
        // The packets held for the mid, oldest first. Empty for a release.
        std::vector<std::vector<uint8_t>> held;
    };

    VoiceSlotMap() : dropped_(std::make_shared<std::atomic<uint64_t>>(0)) {}

    // Count drops in `counter` from now on (the context's
    // MelloDebugStats.rx_unmapped_dropped). A null counter is ignored.
    void set_drop_counter(std::shared_ptr<std::atomic<uint64_t>> counter) {
        if (!counter) {
            return;
        }
        std::lock_guard<std::mutex> lock(mutex_);
        counter->fetch_add(dropped_->load(std::memory_order_relaxed),
                           std::memory_order_relaxed);
        dropped_ = std::move(counter);
    }

    // Bind `mid` to `sender` at `now_ms`. An empty `sender` frees the mid.
    // For a bind, returns the packets held for the mid that are younger than
    // kHoldMs, in arrival order; the caller gives them to `sender`.
    Bound bind(const std::string& mid, const std::string& sender, int64_t now_ms) {
        std::lock_guard<std::mutex> lock(mutex_);
        Slot& slot = slots_[mid];
        Bound out;
        out.previous = std::move(slot.sender);
        slot.sender = sender;
        if (sender.empty()) {
            // Late packets of the old owner can still arrive.
            slot.released = !out.previous.empty() || slot.released;
            return out;
        }
        slot.released = false;
        slot.has_last_ts = false;
        expire_locked(slot, now_ms);
        out.held.reserve(slot.held.size());
        for (auto& h : slot.held) {
            out.held.push_back(std::move(h.bytes));
        }
        slot.held.clear();
        return out;
    }

    // Bind or free `mid` and discard what bind() returns. For tests and
    // callers that do not play held packets: the held packets count as
    // dropped.
    std::string set(const std::string& mid, const std::string& sender, int64_t now_ms = 0) {
        Bound bound = bind(mid, sender, now_ms);
        std::lock_guard<std::mutex> lock(mutex_);
        count_drop_locked(bound.held.size());
        return bound.previous;
    }

    // The sender of one RTP packet on `mid` at `now_ms`. "" when the mid has
    // no owner: the packet is held for its `slot` message, or dropped.
    std::string route(const std::string& mid, const uint8_t* rtp, size_t size, int64_t now_ms) {
        std::lock_guard<std::mutex> lock(mutex_);
        Slot& slot = slots_[mid];
        const bool has_ts = rtp && size >= 12;
        const uint32_t ts = has_ts ? rtp_timestamp(rtp) : 0;
        if (!slot.sender.empty()) {
            if (has_ts) {
                slot.last_ts = ts;
                slot.has_last_ts = true;
            }
            return slot.sender;
        }
        expire_locked(slot, now_ms);
        if (!has_ts) {
            count_drop_locked(1);
            return std::string();
        }
        if (slot.released && slot.has_last_ts
            && is_within_window(ts, slot.last_ts)) {
            count_drop_locked(1);
            return std::string();
        }
        if (slot.held.size() >= kMaxHeldPackets) {
            slot.held.pop_front();
            count_drop_locked(1);
        }
        slot.held.push_back(Held{std::vector<uint8_t>(rtp, rtp + size), now_ms});
        return std::string();
    }

    // The sender that `mid` carries now, "" if none.
    std::string lookup(const std::string& mid) const {
        std::lock_guard<std::mutex> lock(mutex_);
        const auto it = slots_.find(mid);
        return it == slots_.end() ? std::string() : it->second.sender;
    }

    // Count of mids that carry a sender.
    int assigned() const {
        std::lock_guard<std::mutex> lock(mutex_);
        int n = 0;
        for (const auto& entry : slots_) {
            n += entry.second.sender.empty() ? 0 : 1;
        }
        return n;
    }

    // Packets on a mid with no owner that were dropped.
    uint64_t dropped() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return dropped_->load(std::memory_order_relaxed);
    }

    // Packets held now, on all mids.
    size_t held() const {
        std::lock_guard<std::mutex> lock(mutex_);
        size_t n = 0;
        for (const auto& entry : slots_) {
            n += entry.second.held.size();
        }
        return n;
    }

    // Forget every mid. Held packets count as dropped.
    void clear() {
        std::lock_guard<std::mutex> lock(mutex_);
        for (const auto& entry : slots_) {
            count_drop_locked(entry.second.held.size());
        }
        slots_.clear();
    }

private:
    struct Held {
        std::vector<uint8_t> bytes;
        int64_t arrived_ms = 0;
    };

    struct Slot {
        std::string sender;
        std::deque<Held> held;
        // The timestamp of the last packet routed to the owner.
        uint32_t last_ts = 0;
        bool has_last_ts = false;
        // The mid lost its owner: late packets of that owner can follow.
        bool released = false;
    };

    static uint32_t rtp_timestamp(const uint8_t* rtp) {
        return (static_cast<uint32_t>(rtp[4]) << 24) | (static_cast<uint32_t>(rtp[5]) << 16)
            | (static_cast<uint32_t>(rtp[6]) << 8) | static_cast<uint32_t>(rtp[7]);
    }

    static bool is_within_window(uint32_t ts, uint32_t last_ts) {
        const auto d = static_cast<int32_t>(ts - last_ts);
        return d > -kLateWindowSamples && d < kLateWindowSamples;
    }

    void expire_locked(Slot& slot, int64_t now_ms) {
        while (!slot.held.empty() && now_ms - slot.held.front().arrived_ms > kHoldMs) {
            slot.held.pop_front();
            count_drop_locked(1);
        }
    }

    void count_drop_locked(size_t n) {
        if (n > 0) {
            dropped_->fetch_add(n, std::memory_order_relaxed);
        }
    }

    mutable std::mutex mutex_;
    std::unordered_map<std::string, Slot> slots_;
    std::shared_ptr<std::atomic<uint64_t>> dropped_;
};

}  // namespace mello::transport
