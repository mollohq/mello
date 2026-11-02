#include "replay_audio_ring.hpp"
#include "../util/log.hpp"
#include "../util/ring_file.hpp"
#include <algorithm>
#include <chrono>
#include <cstring>
#include <system_error>

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/audio-ring";
constexpr auto kWriterPeriod = std::chrono::milliseconds(20);

// One slot on disk: the header, then kChunkFrames stereo int16 frames.
struct SlotHeader {
    uint64_t seq;
    uint64_t capture_us;
    uint32_t frames;
    uint32_t reserved;
};
constexpr size_t kPayloadBytes =
    ReplayAudioRing::kChunkFrames * ReplayAudioRing::kChannels * sizeof(int16_t);
constexpr size_t kSlotBytes = sizeof(SlotHeader) + kPayloadBytes;

uint64_t frames_to_us(uint64_t frames) {
    return frames * 1'000'000 / ReplayAudioRing::kRate;
}

} // namespace

struct ReplayAudioRing::State {
    util::RingFile file;
    uint32_t slot_count = 0;

    mutable std::mutex mutex;
    std::vector<AudioChunkRef> index;  // per slot: the chunk it holds
    std::vector<bool> filled;          // per slot
    uint64_t next_seq = 0;
    ReplayAudioRing::Stats stats;
};

ReplayAudioRing::~ReplayAudioRing() {
    stop();
}

bool ReplayAudioRing::start(const Config& config) {
    if (active_.load(std::memory_order_acquire) || writer_.joinable()) stop();
    if (config.seconds == 0) return false;

    const std::filesystem::path dir = config.dir.empty() ? util::default_clip_dir() : config.dir;
    std::error_code ec;
    if (dir.empty() || (std::filesystem::create_directories(dir, ec), ec)) {
        MELLO_LOG_ERROR(TAG, "start: no folder for the ring (%s)", dir.string().c_str());
        return false;
    }

    auto state = std::make_shared<State>();
    state->slot_count = config.seconds * 100;
    const uint64_t file_bytes = static_cast<uint64_t>(state->slot_count) * kSlotBytes;
    const std::filesystem::path path = dir / util::ring_file_name("replay_audio");
    if (!state->file.open(path, file_bytes)) {
        MELLO_LOG_ERROR(TAG, "start: cannot make ring file %s", path.string().c_str());
        return false;
    }
    state->index.resize(state->slot_count);
    state->filled.assign(state->slot_count, false);

    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        state_ = std::move(state);
        chunk_.assign(static_cast<size_t>(kChunkFrames) * kChannels, 0);
        chunk_frames_ = 0;
        chunk_us_ = 0;
    }
    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        queue_.clear();
        queued_frames_ = 0;
        dropped_frames_ = 0;
    }
    {
        std::lock_guard<std::mutex> lock(wake_mutex_);
        stop_writer_ = false;
    }
    active_.store(true, std::memory_order_release);
    if (config.writer_thread) writer_ = std::thread([this] { writer_loop(); });

    MELLO_LOG_INFO(TAG, "started: %s (%u s, %llu bytes on disk)", path.string().c_str(),
                   config.seconds, static_cast<unsigned long long>(file_bytes));
    return true;
}

void ReplayAudioRing::stop() {
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
        chunk_frames_ = 0;
    }
    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        queue_.clear();
        queued_frames_ = 0;
    }
    if (was_active && state) {
        std::lock_guard<std::mutex> lock(state->mutex);
        MELLO_LOG_INFO(TAG, "stopped: %llu chunks, %llu frames dropped",
                       static_cast<unsigned long long>(state->stats.chunks_written),
                       static_cast<unsigned long long>(state->stats.frames_dropped));
    }
}

void ReplayAudioRing::push(const int16_t* interleaved, size_t frames, uint64_t capture_us) {
    if (!active_.load(std::memory_order_relaxed) || frames == 0 || !interleaved) return;
    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        if (queued_frames_ + frames > kQueueFrames) {
            dropped_frames_ += frames;
            return;
        }
        queue_.push_back(Packet{std::vector<int16_t>(interleaved, interleaved + frames * kChannels),
                                capture_us});
        queued_frames_ += frames;
    }
}

void ReplayAudioRing::writer_loop() {
    for (;;) {
        {
            std::unique_lock<std::mutex> lock(wake_mutex_);
            wake_.wait_for(lock, kWriterPeriod, [this] { return stop_writer_; });
            if (stop_writer_) return;
        }
        std::lock_guard<std::mutex> lock(file_mutex_);
        drain(false);
    }
}

void ReplayAudioRing::flush() {
    std::lock_guard<std::mutex> lock(file_mutex_);
    drain(true);
}

void ReplayAudioRing::drain(bool partial) {
    std::deque<Packet> packets;
    uint64_t dropped = 0;
    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        packets.swap(queue_);
        queued_frames_ = 0;
        dropped = dropped_frames_;
        dropped_frames_ = 0;
    }
    if (!state_) return;
    if (dropped > 0) {
        std::lock_guard<std::mutex> lock(state_->mutex);
        state_->stats.frames_dropped += dropped;
    }

    for (const Packet& p : packets) {
        const size_t frames = p.samples.size() / kChannels;
        // A packet after a gap starts a new chunk at its own time.
        if (chunk_frames_ > 0) {
            const uint64_t chunk_end = chunk_us_ + frames_to_us(chunk_frames_);
            if (p.capture_us > chunk_end + kGapUs || p.capture_us + kGapUs < chunk_end) {
                if (!write_chunk()) return;
            }
        }
        size_t off = 0;
        while (off < frames) {
            if (chunk_frames_ == 0) chunk_us_ = p.capture_us + frames_to_us(off);
            const size_t n = std::min<size_t>(kChunkFrames - chunk_frames_, frames - off);
            std::memcpy(chunk_.data() + static_cast<size_t>(chunk_frames_) * kChannels,
                        p.samples.data() + off * kChannels, n * kChannels * sizeof(int16_t));
            chunk_frames_ += static_cast<uint32_t>(n);
            off += n;
            if (chunk_frames_ == kChunkFrames && !write_chunk()) return;
        }
    }
    if (partial && chunk_frames_ > 0) write_chunk();
}

bool ReplayAudioRing::write_chunk() {
    State& st = *state_;
    uint64_t seq = 0;
    {
        std::lock_guard<std::mutex> lock(st.mutex);
        seq = st.next_seq;
    }
    const uint32_t slot = static_cast<uint32_t>(seq % st.slot_count);

    uint8_t buf[kSlotBytes];
    SlotHeader header{seq, chunk_us_, chunk_frames_, 0};
    std::memcpy(buf, &header, sizeof(header));
    std::memset(buf + sizeof(header), 0, kPayloadBytes);
    std::memcpy(buf + sizeof(header), chunk_.data(),
                static_cast<size_t>(chunk_frames_) * kChannels * sizeof(int16_t));

    // Out of the index first: a window must not list the slot while its bytes
    // change.
    {
        std::lock_guard<std::mutex> lock(st.mutex);
        st.filled[slot] = false;
    }
    if (!st.file.write_at(static_cast<uint64_t>(slot) * kSlotBytes, buf, kSlotBytes)) {
        MELLO_LOG_ERROR(TAG, "ring file write failed, replay audio ring off");
        active_.store(false, std::memory_order_release);
        chunk_frames_ = 0;
        return false;
    }
    {
        std::lock_guard<std::mutex> lock(st.mutex);
        st.index[slot] = AudioChunkRef{seq, chunk_us_, chunk_frames_};
        st.filled[slot] = true;
        st.next_seq = seq + 1;
        ++st.stats.chunks_written;
    }
    chunk_frames_ = 0;
    return true;
}

ReplayAudioRing::Window ReplayAudioRing::window(uint64_t start_us, uint64_t end_us) {
    Window window;
    std::shared_ptr<State> state;
    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        state = state_;
    }
    if (!state) return window;

    std::lock_guard<std::mutex> lock(state->mutex);
    const uint64_t first = state->next_seq > state->slot_count ? state->next_seq - state->slot_count : 0;
    for (uint64_t seq = first; seq < state->next_seq; ++seq) {
        const uint32_t slot = static_cast<uint32_t>(seq % state->slot_count);
        if (!state->filled[slot]) continue;
        const AudioChunkRef& c = state->index[slot];
        if (c.seq != seq) continue;
        const uint64_t c_end = c.capture_us + frames_to_us(c.frames);
        if (c_end > start_us && c.capture_us < end_us) window.chunks_.push_back(c);
    }
    window.state_ = std::move(state);
    return window;
}

ReplayAudioRing::Stats ReplayAudioRing::stats() const {
    std::shared_ptr<State> state;
    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        state = state_;
    }
    if (!state) return {};
    std::lock_guard<std::mutex> lock(state->mutex);
    return state->stats;
}

bool ReplayAudioRing::Window::read(const AudioChunkRef& chunk, int16_t* out) const {
    if (!state_ || !out || chunk.frames > kChunkFrames) return false;
    uint8_t buf[kSlotBytes];
    const uint32_t slot = static_cast<uint32_t>(chunk.seq % state_->slot_count);
    if (!state_->file.read_at(static_cast<uint64_t>(slot) * kSlotBytes, buf, kSlotBytes)) {
        return false;
    }
    SlotHeader header;
    std::memcpy(&header, buf, sizeof(header));
    // The writer reused the slot: the chunk is gone.
    if (header.seq != chunk.seq || header.frames != chunk.frames) return false;
    std::memcpy(out, buf + sizeof(header),
                static_cast<size_t>(chunk.frames) * kChannels * sizeof(int16_t));
    return true;
}

} // namespace mello::replay
