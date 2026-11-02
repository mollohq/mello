#include "clip_buffer.hpp"
#include "opus_codec.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <system_error>

namespace mello::audio {

namespace {

// The crew mix holds voices and stream game audio: a music-capable Opus mode
// at the voice bitrate. The mic uses the same.
constexpr int kRingBitrate = 64000;
constexpr size_t kPendingSeconds = 2;
// One anchor per write (10 ms on the render thread, 10 or 20 ms on the
// capture thread), with room to spare.
constexpr size_t kAnchorQueue = kPendingSeconds * 100 + 56;
constexpr auto kWriterPeriod = std::chrono::milliseconds(20);
constexpr uint64_t kNoTime = UINT64_MAX;

void put16(uint8_t* p, uint16_t v) {
    p[0] = static_cast<uint8_t>(v & 0xFF);
    p[1] = static_cast<uint8_t>(v >> 8);
}
uint16_t get16(const uint8_t* p) { return static_cast<uint16_t>(p[0] | (p[1] << 8)); }
void put64(uint8_t* p, uint64_t v) {
    for (int i = 0; i < 8; ++i) p[i] = static_cast<uint8_t>(v >> (8 * i));
}
uint64_t get64(const uint8_t* p) {
    uint64_t v = 0;
    for (int i = 7; i >= 0; --i) v = (v << 8) | p[i];
    return v;
}

} // namespace

ClipBuffer::Stream::Stream(const char* n, size_t pending_samples)
    : name(n), pending(pending_samples), anchors(kAnchorQueue) {}

ClipBuffer::ClipBuffer(int sample_rate, int buffer_seconds, std::string dir)
    : sample_rate_(sample_rate)
    , frame_samples_(static_cast<size_t>(sample_rate) / 50)
    , slot_count_(static_cast<size_t>(buffer_seconds) * 50)
    , dir_(std::move(dir))
    , crew_("crew", static_cast<size_t>(sample_rate) * kPendingSeconds)
    , mic_("mic", static_cast<size_t>(sample_rate) * kPendingSeconds)
    , frame_(static_cast<size_t>(sample_rate) / 50) {}

ClipBuffer::~ClipBuffer() {
    stop();
}

bool ClipBuffer::open_stream(Stream& s, const std::filesystem::path& dir, const char* prefix) {
    const std::filesystem::path path = dir / util::ring_file_name(prefix);
    auto file = std::make_unique<RingFile>();
    if (!file->open(path, static_cast<uint64_t>(slot_count_) * kSlotBytes)) {
        MELLO_LOG_ERROR("clip_buffer", "start: cannot make ring file %s", path.string().c_str());
        return false;
    }
    auto encoder = std::make_unique<OpusEnc>();
    if (!encoder->initialize(sample_rate_, 1, kRingBitrate, OpusApplication::Audio)) {
        MELLO_LOG_ERROR("clip_buffer", "start: Opus encoder init failed");
        return false;
    }
    s.file = std::move(file);
    s.encoder = std::move(encoder);
    s.anchors_seen.clear();
    s.samples_read = 0;
    s.samples_written = 0;
    s.frames_written = 0;
    s.slot_time.assign(slot_count_, kNoTime);
    s.pending.clear();
    s.anchors.clear();
    s.dropped.store(0, std::memory_order_relaxed);
    return true;
}

bool ClipBuffer::start() {
    // A second start (a rejoin) begins a new, empty buffer.
    if (active_.load(std::memory_order_acquire) || writer_.joinable()) stop();

    const std::filesystem::path dir = dir_.empty() ? util::default_clip_dir() : std::filesystem::path(dir_);
    if (dir.empty()) {
        MELLO_LOG_ERROR("clip_buffer", "start: no temp folder");
        return false;
    }
    std::error_code ec;
    std::filesystem::create_directories(dir, ec);
    if (ec) {
        MELLO_LOG_ERROR("clip_buffer", "start: cannot make %s: %s",
                        dir.string().c_str(), ec.message().c_str());
        return false;
    }
    const auto space = std::filesystem::space(dir, ec);
    if (ec || space.available < kMinFreeBytes) {
        MELLO_LOG_WARN("clip_buffer", "start: off, %llu MB free in %s (need %llu MB)",
                       ec ? 0ull : static_cast<unsigned long long>(space.available >> 20),
                       dir.string().c_str(),
                       static_cast<unsigned long long>(kMinFreeBytes >> 20));
        return false;
    }

    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        if (!open_stream(crew_, dir, "voice_ring") || !open_stream(mic_, dir, "voice_mic")) {
            for (Stream* s : {&crew_, &mic_}) {
                s->file.reset();
                s->encoder.reset();
            }
            return false;
        }
        lookahead_us_ =
            static_cast<uint64_t>(crew_.encoder->lookahead()) * 1'000'000 / sample_rate_;
    }
    {
        std::lock_guard<std::mutex> lock(wake_mutex_);
        stop_writer_ = false;
    }
    active_.store(true, std::memory_order_release);
    writer_ = std::thread([this] { writer_loop(); });

    MELLO_LOG_INFO("clip_buffer", "started in %s (2 streams of %zu frames, %.1fs, %llu bytes on disk)",
                   dir.string().c_str(), slot_count_,
                   static_cast<float>(slot_count_ * frame_samples_) / sample_rate_,
                   static_cast<unsigned long long>(ring_bytes()));
    return true;
}

void ClipBuffer::stop() {
    const bool was_active = active_.exchange(false, std::memory_order_acq_rel);
    {
        std::lock_guard<std::mutex> lock(wake_mutex_);
        stop_writer_ = true;
    }
    wake_.notify_all();
    if (writer_.joinable()) writer_.join();

    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        for (Stream* s : {&crew_, &mic_}) {
            s->file.reset();  // closing deletes the ring file
            s->encoder.reset();
            s->frames_written = 0;
            s->pending.clear();
            s->anchors.clear();
        }
    }
    if (was_active) {
        MELLO_LOG_INFO("clip_buffer", "stopped (dropped %llu crew, %llu mic samples)",
                       static_cast<unsigned long long>(crew_.dropped.load(std::memory_order_relaxed)),
                       static_cast<unsigned long long>(mic_.dropped.load(std::memory_order_relaxed)));
    }
}

uint64_t ClipBuffer::dropped_samples() const {
    return crew_.dropped.load(std::memory_order_relaxed) + mic_.dropped.load(std::memory_order_relaxed);
}

void ClipBuffer::write(Stream& s, const int16_t* samples, size_t count, uint64_t time_us,
                       float volume) {
    if (!active_.load(std::memory_order_relaxed) || count == 0) return;
    // The anchor goes first, so the writer has it when the samples come.
    const Anchor anchor{s.samples_written, time_us, volume};
    if (s.anchors.write(&anchor, 1) == 0) {
        s.dropped.fetch_add(count, std::memory_order_relaxed);
        return;
    }
    const size_t queued = s.pending.write(samples, count);
    s.samples_written += queued;
    if (queued < count) {
        s.dropped.fetch_add(count - queued, std::memory_order_relaxed);
    }
}

void ClipBuffer::write_crew(const int16_t* samples, size_t count, uint64_t heard_us, float volume) {
    write(crew_, samples, count, heard_us, std::clamp(volume, 0.0f, 1.0f));
}

void ClipBuffer::write_mic(const int16_t* samples, size_t count, uint64_t capture_us) {
    write(mic_, samples, count, capture_us, 1.0f);
}

void ClipBuffer::writer_loop() {
    for (;;) {
        {
            std::unique_lock<std::mutex> lock(wake_mutex_);
            wake_.wait_for(lock, kWriterPeriod, [this] { return stop_writer_; });
            if (stop_writer_) return;
        }
        std::lock_guard<std::mutex> lock(file_mutex_);
        drain_locked(crew_);
        drain_locked(mic_);
    }
}

void ClipBuffer::flush() {
    std::lock_guard<std::mutex> lock(file_mutex_);
    drain_locked(crew_);
    drain_locked(mic_);
}

void ClipBuffer::drain_locked(Stream& s) {
    if (!s.file || !s.encoder || !active_.load(std::memory_order_relaxed)) return;

    Anchor a{};
    while (s.anchors.read(&a, 1) == 1) s.anchors_seen.push_back(a);

    uint8_t slot[kSlotBytes];
    while (s.pending.available() >= frame_samples_) {
        s.pending.read(frame_.data(), frame_samples_);
        // The newest anchor at or before the frame's first sample.
        while (s.anchors_seen.size() > 1 && s.anchors_seen[1].sample_index <= s.samples_read) {
            s.anchors_seen.pop_front();
        }
        uint64_t time_us = 0;
        float volume = 1.0f;
        if (!s.anchors_seen.empty()) {
            const Anchor& at = s.anchors_seen.front();
            const double offset_us = (static_cast<double>(s.samples_read) -
                                      static_cast<double>(at.sample_index)) *
                                     1e6 / sample_rate_;
            time_us = static_cast<uint64_t>(std::llround(static_cast<double>(at.time_us) + offset_us));
            volume = at.volume;
        }
        s.samples_read += frame_samples_;

        int len = s.encoder->encode(frame_.data(), static_cast<int>(frame_samples_),
                                    slot + kSlotHeaderBytes, static_cast<int>(kMaxPacketBytes));
        if (len < 0) {
            // An empty slot decodes as concealment. The ring keeps its time.
            MELLO_LOG_WARN("clip_buffer", "Opus encode failed (%d) on %s", len, s.name);
            len = 0;
        }
        put16(slot, static_cast<uint16_t>(len));
        put64(slot + 2, time_us);
        put16(slot + 10, static_cast<uint16_t>(std::lround(volume * 65535.0f)));
        const size_t index = static_cast<size_t>(s.frames_written % slot_count_);
        if (!s.file->write_at(static_cast<uint64_t>(index) * kSlotBytes, slot, kSlotBytes)) {
            // CLIPS.md 4.4: a full or failing disk turns the buffer off.
            MELLO_LOG_ERROR("clip_buffer", "ring file write failed, buffer off");
            active_.store(false, std::memory_order_release);
            return;
        }
        s.slot_time[index] = time_us;
        ++s.frames_written;
    }
}

bool ClipBuffer::read_frames(Stream& s, uint64_t start_us, uint64_t end_us,
                             std::vector<replay::TimedPcm>& out,
                             std::vector<std::pair<uint64_t, float>>* volume) {
    if (!s.file) return false;
    const uint64_t available = std::min<uint64_t>(s.frames_written, slot_count_);
    if (available == 0) return true;
    const uint64_t frame_us = static_cast<uint64_t>(frame_samples_) * 1'000'000 / sample_rate_;

    // The frames that overlap the window.
    const uint64_t oldest = s.frames_written - available;
    uint64_t first = UINT64_MAX;
    uint64_t last = 0;
    for (uint64_t i = oldest; i < s.frames_written; ++i) {
        const uint64_t t = s.slot_time[static_cast<size_t>(i % slot_count_)];
        if (t == kNoTime || t + frame_us <= start_us || t >= end_us) continue;
        if (first == UINT64_MAX) first = i;
        last = i;
    }
    if (first == UINT64_MAX) return true;
    // One frame before the window primes the decoder.
    const bool primer = first > oldest;
    const uint64_t from = primer ? first - 1 : first;

    OpusDec decoder;
    if (!decoder.initialize(sample_rate_, 1)) {
        MELLO_LOG_ERROR("clip_buffer", "read: Opus decoder init failed");
        return false;
    }
    uint8_t slot[kSlotBytes];
    std::vector<int16_t> pcm(frame_samples_);
    float last_volume = -1.0f;
    for (uint64_t i = from; i <= last; ++i) {
        if (!s.file->read_at((i % slot_count_) * kSlotBytes, slot, kSlotBytes)) {
            MELLO_LOG_ERROR("clip_buffer", "read: ring file read failed");
            return false;
        }
        size_t len = get16(slot);
        if (len > kMaxPacketBytes) len = 0;  // a bad slot: conceal it
        const uint64_t t = get64(slot + 2);
        const float v = static_cast<float>(get16(slot + 10)) / 65535.0f;
        int n = len == 0 ? decoder.decode_plc(pcm.data(), static_cast<int>(frame_samples_))
                         : decoder.decode(slot + kSlotHeaderBytes, static_cast<int>(len), pcm.data(),
                                          static_cast<int>(frame_samples_));
        if (n < 0) {
            std::fill(pcm.begin(), pcm.end(), int16_t{0});
            n = static_cast<int>(frame_samples_);
        }
        if (primer && i == from) continue;
        // The decoder's output lags its input by the encoder's lookahead.
        const uint64_t at = t > lookahead_us_ ? t - lookahead_us_ : 0;
        replay::TimedPcm frame;
        frame.capture_us = at;
        frame.samples.assign(pcm.begin(), pcm.begin() + n);
        out.push_back(std::move(frame));
        if (volume && v != last_volume) {
            volume->emplace_back(at, v);
            last_volume = v;
        }
    }
    return true;
}

bool ClipBuffer::read_window(uint64_t start_us, uint64_t end_us, VoiceWindow& out) {
    std::lock_guard<std::mutex> lock(file_mutex_);
    if (!active_.load(std::memory_order_relaxed) || !crew_.file) return false;
    drain_locked(crew_);
    drain_locked(mic_);
    out = VoiceWindow{};
    return read_frames(crew_, start_us, end_us, out.crew, &out.crew_volume) &&
           read_frames(mic_, start_us, end_us, out.mic, nullptr);
}

bool ClipBuffer::capture(float seconds, const std::string& output_path) {
    if (seconds <= 0.0f || output_path.empty()) {
        MELLO_LOG_WARN("clip_buffer", "capture: invalid params (seconds=%.1f)", seconds);
        return false;
    }
    const uint64_t frame_us = static_cast<uint64_t>(frame_samples_) * 1'000'000 / sample_rate_;

    uint64_t start_us = 0;
    uint64_t end_us = 0;
    VoiceWindow window;
    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        if (!crew_.file || !active_.load(std::memory_order_relaxed)) {
            MELLO_LOG_WARN("clip_buffer", "capture: buffer not active");
            return false;
        }
        // Take the newest audio too, not only what the writer saw last.
        drain_locked(crew_);
        drain_locked(mic_);

        // The window ends at the newest crew audio (the render thread writes
        // it all the time in voice), else at the newest mic audio.
        Stream& ref = crew_.frames_written > 0 ? crew_ : mic_;
        if (ref.frames_written == 0) {
            MELLO_LOG_WARN("clip_buffer", "capture: no audio in buffer");
            return false;
        }
        const uint64_t available = std::min<uint64_t>(ref.frames_written, slot_count_);
        const uint64_t newest = ref.slot_time[static_cast<size_t>((ref.frames_written - 1) % slot_count_)];
        const uint64_t oldest =
            ref.slot_time[static_cast<size_t>((ref.frames_written - available) % slot_count_)];
        end_us = newest + frame_us;
        const auto want_us = static_cast<uint64_t>(static_cast<double>(seconds) * 1e6);
        start_us = end_us > want_us ? std::max(end_us - want_us, oldest) : oldest;

        if (!read_frames(crew_, start_us, end_us, window.crew, nullptr) ||
            !read_frames(mic_, start_us, end_us, window.mic, nullptr)) {
            return false;
        }
    }

    // Both streams at their own times, then one channel: they are mono, so
    // both channels of the mix are the same.
    replay::ClipAudioSource crew;
    crew.channels = 1;
    crew.chunks = std::move(window.crew);
    replay::ClipAudioSource mic;
    mic.channels = 1;
    mic.chunks = std::move(window.mic);
    const std::vector<int16_t> stereo = replay::mix_clip_audio(start_us, end_us, {crew, mic});
    std::vector<int16_t> pcm(stereo.size() / 2);
    for (size_t i = 0; i < pcm.size(); ++i) pcm[i] = stereo[2 * i];

    MELLO_LOG_INFO("clip_buffer", "capture: extracted %zu samples (%.1fs) from the ring files",
                   pcm.size(), static_cast<float>(pcm.size()) / sample_rate_);

    if (!write_wav(output_path, pcm.data(), pcm.size(), sample_rate_)) {
        MELLO_LOG_ERROR("clip_buffer", "capture: failed to write WAV to %s", output_path.c_str());
        return false;
    }

    MELLO_LOG_INFO("clip_buffer", "capture: saved %s (%.1fs, %zu bytes)",
                   output_path.c_str(), static_cast<float>(pcm.size()) / sample_rate_,
                   pcm.size() * sizeof(int16_t) + 44);
    return true;
}

bool ClipBuffer::write_wav(const std::string& path, const int16_t* data,
                           size_t sample_count, int sample_rate) {
    std::ofstream file(path, std::ios::binary);
    if (!file) return false;

    uint32_t data_size = static_cast<uint32_t>(sample_count * sizeof(int16_t));
    uint32_t file_size = data_size + 36;
    uint16_t channels = 1;
    uint16_t bits_per_sample = 16;
    uint32_t sr = static_cast<uint32_t>(sample_rate);
    uint32_t byte_rate = sr * channels * bits_per_sample / 8;
    uint16_t block_align = channels * bits_per_sample / 8;
    uint32_t fmt_size = 16;
    uint16_t audio_format = 1; // PCM

    file.write("RIFF", 4);
    file.write(reinterpret_cast<const char*>(&file_size), 4);
    file.write("WAVE", 4);

    file.write("fmt ", 4);
    file.write(reinterpret_cast<const char*>(&fmt_size), 4);
    file.write(reinterpret_cast<const char*>(&audio_format), 2);
    file.write(reinterpret_cast<const char*>(&channels), 2);
    file.write(reinterpret_cast<const char*>(&sr), 4);
    file.write(reinterpret_cast<const char*>(&byte_rate), 4);
    file.write(reinterpret_cast<const char*>(&block_align), 2);
    file.write(reinterpret_cast<const char*>(&bits_per_sample), 2);

    file.write("data", 4);
    file.write(reinterpret_cast<const char*>(&data_size), 4);
    file.write(reinterpret_cast<const char*>(data), data_size);

    return file.good();
}

} // namespace mello::audio
