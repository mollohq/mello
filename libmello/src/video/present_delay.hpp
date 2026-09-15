#pragma once
#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>

namespace mello::video {

/// Delay from the moment a frame was presented to the moment the capture
/// backend received it, in 1 ms buckets. Cumulative; readers diff snapshots.
///
/// Used by the DXGI vs WGC benchmark (streaming reliability plan 2.6) to
/// compare capture latency. Written on capture threads, read by stats callers.
class PresentDelayHistogram {
public:
    static constexpr size_t kBuckets = 32;  // bucket i = [i, i+1) ms; last = >= 31 ms

    static size_t bucket_for_ms(double ms) {
        if (!(ms > 0.0)) return 0;
        const size_t b = static_cast<size_t>(ms);
        return b < kBuckets ? b : kBuckets - 1;
    }

    void record_ms(double ms) {
        counts_[bucket_for_ms(ms)].fetch_add(1, std::memory_order_relaxed);
    }

    void snapshot(uint32_t* out) const {
        for (size_t i = 0; i < kBuckets; ++i) {
            out[i] = counts_[i].load(std::memory_order_relaxed);
        }
    }

private:
    std::array<std::atomic<uint32_t>, kBuckets> counts_{};
};

}  // namespace mello::video
