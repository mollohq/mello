#include "replay_video_ring.hpp"
#include "../util/log.hpp"
#include "../util/ring_file.hpp"
#include <algorithm>
#include <chrono>
#include <system_error>

namespace mello::replay {

namespace {
constexpr const char* TAG = "replay/video-ring";
constexpr auto kWriterPeriod = std::chrono::milliseconds(10);
} // namespace

struct ReplayVideoRing::State {
    struct Slot {
        uint32_t pins = 0;
        bool     holds_segment = false;
        uint64_t segment_seq = 0;
    };
    struct Segment {
        uint64_t seq = 0;
        uint32_t slot = 0;
        uint64_t used = 0;  // bytes written into the slot
        std::vector<AccessUnitRef> units;
    };

    util::RingFile file;
    uint64_t segment_bytes = 0;

    // Guards everything below. The writer takes it for index updates only,
    // never across a file write.
    mutable std::mutex mutex;
    std::vector<Slot> slots;
    std::deque<Segment> segments;  // oldest first
    bool writing = false;          // the newest segment takes units
    uint32_t next_slot = 0;
    uint64_t next_seq = 0;
    ReplayVideoRing::Stats stats;
};

uint64_t ReplayVideoRing::segment_bytes_for(uint32_t bitrate_kbps) {
    const uint64_t bytes_per_s = static_cast<uint64_t>(bitrate_kbps) * 1000 / 8;
    const uint64_t peak = bytes_per_s + bytes_per_s / 4;
    return 2 * peak;
}

ReplayVideoRing::~ReplayVideoRing() {
    stop();
}

bool ReplayVideoRing::start(const Config& config) {
    if (active_.load(std::memory_order_acquire) || writer_.joinable()) stop();
    if (config.segments < 2 || config.segment_bytes == 0) {
        MELLO_LOG_ERROR(TAG, "start: bad config (segments=%u segment_bytes=%llu)", config.segments,
                        static_cast<unsigned long long>(config.segment_bytes));
        return false;
    }

    const std::filesystem::path dir = config.dir.empty() ? util::default_clip_dir() : config.dir;
    std::error_code ec;
    if (dir.empty() || (std::filesystem::create_directories(dir, ec), ec)) {
        MELLO_LOG_ERROR(TAG, "start: no folder for the ring (%s)", dir.string().c_str());
        return false;
    }

    auto state = std::make_shared<State>();
    const std::filesystem::path path = dir / util::ring_file_name("replay_video");
    const uint64_t file_bytes = static_cast<uint64_t>(config.segments) * config.segment_bytes;
    if (!state->file.open(path, file_bytes)) {
        MELLO_LOG_ERROR(TAG, "start: cannot make ring file %s (%llu bytes)", path.string().c_str(),
                        static_cast<unsigned long long>(file_bytes));
        return false;
    }
    state->segment_bytes = config.segment_bytes;
    state->slots.resize(config.segments);

    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        state_ = std::move(state);
        write_skipping_ = false;
    }
    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        queue_.clear();
        queued_bytes_ = 0;
        queue_limit_ = config.queue_bytes;
        push_dropping_ = false;
    }
    keyframe_request_.store(false, std::memory_order_relaxed);
    {
        std::lock_guard<std::mutex> lock(wake_mutex_);
        stop_writer_ = false;
    }
    active_.store(true, std::memory_order_release);
    if (config.writer_thread) writer_ = std::thread([this] { writer_loop(); });

    MELLO_LOG_INFO(TAG, "started: %s (%u segments of %llu bytes, %llu MB on disk)",
                   path.string().c_str(), config.segments,
                   static_cast<unsigned long long>(config.segment_bytes),
                   static_cast<unsigned long long>(file_bytes >> 20));
    return true;
}

void ReplayVideoRing::stop() {
    const bool was_active = active_.exchange(false, std::memory_order_acq_rel);
    {
        std::lock_guard<std::mutex> lock(wake_mutex_);
        stop_writer_ = true;
    }
    wake_.notify_all();
    if (writer_.joinable()) writer_.join();

    std::shared_ptr<State> state;
    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        state = std::move(state_);
    }
    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        queue_.clear();
        queued_bytes_ = 0;
    }
    if (was_active && state) {
        const Stats s = stats_of(*state);
        MELLO_LOG_INFO(TAG,
                       "stopped: %llu units, %llu segments, drops: queue=%llu overflow=%llu "
                       "no-slot=%llu, pinned skips=%llu",
                       static_cast<unsigned long long>(s.units_written),
                       static_cast<unsigned long long>(s.segments_written),
                       static_cast<unsigned long long>(s.queue_drops),
                       static_cast<unsigned long long>(s.overflow_drops),
                       static_cast<unsigned long long>(s.no_slot_drops),
                       static_cast<unsigned long long>(s.pinned_skips));
    }
    // A window that still lives holds the state and the file.
}

void ReplayVideoRing::push(std::vector<uint8_t>&& data, uint64_t capture_us, bool keyframe) {
    if (!active_.load(std::memory_order_relaxed) || data.empty()) return;
    bool queued = false;
    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        if (push_dropping_ && keyframe) push_dropping_ = false;
        if (!push_dropping_ && queued_bytes_ + data.size() > queue_limit_) {
            push_dropping_ = true;
            keyframe_request_.store(true, std::memory_order_release);
        }
        if (push_dropping_) {
            ++dropped_in_queue_;
        } else {
            queued_bytes_ += data.size();
            queue_.push_back(QueuedUnit{std::move(data), capture_us, keyframe});
            queued = true;
        }
    }
    if (queued) wake_.notify_one();
}

void ReplayVideoRing::writer_loop() {
    for (;;) {
        {
            std::unique_lock<std::mutex> lock(wake_mutex_);
            wake_.wait_for(lock, kWriterPeriod, [this] { return stop_writer_; });
            if (stop_writer_) return;
        }
        std::lock_guard<std::mutex> lock(file_mutex_);
        drain();
    }
}

void ReplayVideoRing::flush() {
    std::lock_guard<std::mutex> lock(file_mutex_);
    drain();
}

void ReplayVideoRing::drain() {
    std::deque<QueuedUnit> units;
    uint64_t dropped = 0;
    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        units.swap(queue_);
        queued_bytes_ = 0;
        dropped = dropped_in_queue_;
        dropped_in_queue_ = 0;
    }
    if (!state_) return;
    if (dropped > 0) {
        std::lock_guard<std::mutex> lock(state_->mutex);
        state_->stats.queue_drops += dropped;
    }
    for (auto& unit : units) {
        if (!active_.load(std::memory_order_relaxed)) return;
        write_unit(unit.data, unit.capture_us, unit.keyframe);
    }
}

void ReplayVideoRing::write_unit(std::vector<uint8_t>& data, uint64_t capture_us, bool keyframe) {
    State& st = *state_;
    const uint64_t size = data.size();

    uint64_t offset = 0;
    uint64_t segment_seq = 0;
    {
        std::lock_guard<std::mutex> lock(st.mutex);
        if (keyframe) {
            // A keyframe always opens a new segment. Take the next slot that
            // no window pins, and drop the segment that the slot held.
            st.writing = false;
            uint32_t slot = 0;
            bool found = false;
            for (uint32_t i = 0; i < st.slots.size(); ++i) {
                slot = (st.next_slot + i) % static_cast<uint32_t>(st.slots.size());
                if (st.slots[slot].pins == 0) {
                    found = true;
                    if (i > 0) ++st.stats.pinned_skips;
                    break;
                }
            }
            if (!found) {
                ++st.stats.no_slot_drops;
                write_skipping_ = true;
                keyframe_request_.store(true, std::memory_order_release);
                return;
            }
            if (st.slots[slot].holds_segment) {
                const uint64_t old = st.slots[slot].segment_seq;
                st.segments.erase(std::remove_if(st.segments.begin(), st.segments.end(),
                                                 [old](const State::Segment& s) {
                                                     return s.seq == old;
                                                 }),
                                  st.segments.end());
            }
            State::Segment seg;
            seg.seq = st.next_seq++;
            seg.slot = slot;
            st.segments.push_back(std::move(seg));
            st.slots[slot].holds_segment = true;
            st.slots[slot].segment_seq = st.segments.back().seq;
            st.next_slot = (slot + 1) % static_cast<uint32_t>(st.slots.size());
            st.writing = true;
            write_skipping_ = false;
            ++st.stats.segments_written;
        } else if (!st.writing || write_skipping_) {
            // No segment to append to: the unit has no reference to decode
            // from. It is counted where the gap began.
            return;
        }

        State::Segment& seg = st.segments.back();
        if (seg.used + size > st.segment_bytes) {
            // The GOP is larger than the slot: end the segment here and wait
            // for the next IDR. A conforming encoder never gets here.
            ++st.stats.overflow_drops;
            st.writing = false;
            write_skipping_ = true;
            keyframe_request_.store(true, std::memory_order_release);
            return;
        }
        offset = static_cast<uint64_t>(seg.slot) * st.segment_bytes + seg.used;
        segment_seq = seg.seq;
    }

    // The bytes go to the file before the index lists the unit, so a window
    // never lists a unit whose bytes are not written. The slot is the
    // writer's: no window pins a segment's unwritten tail.
    if (!st.file.write_at(offset, data.data(), static_cast<size_t>(size))) {
        MELLO_LOG_ERROR(TAG, "ring file write failed, replay ring off");
        active_.store(false, std::memory_order_release);
        return;
    }

    std::lock_guard<std::mutex> lock(st.mutex);
    if (st.segments.empty() || st.segments.back().seq != segment_seq) return;
    State::Segment& seg = st.segments.back();
    AccessUnitRef ref;
    ref.segment_seq = segment_seq;
    ref.offset = offset;
    ref.size = static_cast<uint32_t>(size);
    ref.capture_us = capture_us;
    ref.keyframe = keyframe;
    seg.units.push_back(ref);
    seg.used += size;
    ++st.stats.units_written;
}

ReplayVideoRing::Window ReplayVideoRing::pin_window(uint64_t start_us, uint64_t end_us) {
    Window window;
    std::shared_ptr<State> state;
    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        state = state_;
    }
    if (!state) return window;

    std::lock_guard<std::mutex> lock(state->mutex);
    // T0: the newest segment that starts at or before `start_us`. Without
    // one, the oldest segment.
    size_t first = 0;
    for (size_t i = 0; i < state->segments.size(); ++i) {
        const auto& seg = state->segments[i];
        if (seg.units.empty()) continue;
        if (seg.units.front().capture_us <= start_us) first = i;
    }
    for (size_t i = first; i < state->segments.size(); ++i) {
        const auto& seg = state->segments[i];
        bool pinned = false;
        for (const auto& unit : seg.units) {
            if (unit.capture_us > end_us) break;
            if (!pinned) {
                ++state->slots[seg.slot].pins;
                window.slots_.push_back(seg.slot);
                pinned = true;
            }
            window.units_.push_back(unit);
        }
    }
    window.state_ = std::move(state);
    return window;
}

ReplayVideoRing::Stats ReplayVideoRing::stats() const {
    std::shared_ptr<State> state;
    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        state = state_;
    }
    if (!state) return {};
    return stats_of(*state);
}

ReplayVideoRing::Stats ReplayVideoRing::stats_of(const State& state) {
    std::lock_guard<std::mutex> lock(state.mutex);
    return state.stats;
}

// --- Window ---

ReplayVideoRing::Window::~Window() {
    release();
}

ReplayVideoRing::Window::Window(Window&& other) noexcept
    : state_(std::move(other.state_)), slots_(std::move(other.slots_)),
      units_(std::move(other.units_)) {
    other.slots_.clear();
    other.units_.clear();
}

ReplayVideoRing::Window& ReplayVideoRing::Window::operator=(Window&& other) noexcept {
    if (this != &other) {
        release();
        state_ = std::move(other.state_);
        slots_ = std::move(other.slots_);
        units_ = std::move(other.units_);
        other.slots_.clear();
        other.units_.clear();
    }
    return *this;
}

void ReplayVideoRing::Window::release() {
    if (state_) {
        std::lock_guard<std::mutex> lock(state_->mutex);
        for (const uint32_t slot : slots_) {
            if (state_->slots[slot].pins > 0) --state_->slots[slot].pins;
        }
    }
    slots_.clear();
    units_.clear();
    state_.reset();
}

bool ReplayVideoRing::Window::read(const AccessUnitRef& unit, std::vector<uint8_t>& out) const {
    if (!state_) return false;
    out.resize(unit.size);
    return state_->file.read_at(unit.offset, out.data(), unit.size);
}

} // namespace mello::replay
