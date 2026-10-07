#pragma once

#include <mutex>
#include <string>
#include <unordered_map>

namespace mello::transport {

// Voice receive slots (spec 10 section 7.1, mello-sfu 01-SFU.md section 5.4).
// The SFU names the member that each receive m-line carries, by mid, in
// `slot` control messages. The track callback resolves the sender of each
// RTP packet here, so no SDP attribute (msid) names the sender.
//
// Thread safe. set() runs on mello-core's control-channel callback thread,
// lookup() on the libdatachannel track thread for every packet.
class VoiceSlotMap {
public:
    // Bind `mid` to `sender`. An empty `sender` frees the mid. Returns the
    // previous sender of `mid`, "" if it had none.
    std::string set(const std::string& mid, const std::string& sender) {
        std::lock_guard<std::mutex> lock(mutex_);
        std::string previous;
        const auto it = senders_.find(mid);
        if (it != senders_.end()) {
            previous = std::move(it->second);
            senders_.erase(it);
        }
        if (!sender.empty()) {
            senders_.emplace(mid, sender);
        }
        return previous;
    }

    // The sender that `mid` carries now, "" if none: drop the packet.
    std::string lookup(const std::string& mid) const {
        std::lock_guard<std::mutex> lock(mutex_);
        const auto it = senders_.find(mid);
        return it == senders_.end() ? std::string() : it->second;
    }

    // Count of mids that carry a sender.
    int assigned() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return static_cast<int>(senders_.size());
    }

    void clear() {
        std::lock_guard<std::mutex> lock(mutex_);
        senders_.clear();
    }

private:
    mutable std::mutex mutex_;
    std::unordered_map<std::string, std::string> senders_;
};

}  // namespace mello::transport
