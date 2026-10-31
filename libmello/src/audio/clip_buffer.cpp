#include "clip_buffer.hpp"
#include "opus_codec.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <chrono>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <system_error>

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#else
#include <fcntl.h>
#include <unistd.h>
#endif

namespace mello::audio {

namespace {

// The ring holds the mix of voices and stream game audio: a music-capable
// Opus mode at the voice bitrate.
constexpr int kRingBitrate = 64000;
constexpr size_t kPendingSeconds = 2;
constexpr auto kWriterPeriod = std::chrono::milliseconds(20);

std::filesystem::path default_dir() {
    std::error_code ec;
    auto tmp = std::filesystem::temp_directory_path(ec);
    if (ec) return {};
    return tmp / "mello_clips";
}

std::string ring_file_name() {
    static std::atomic<uint32_t> seq{0};
#ifdef _WIN32
    const unsigned long pid = GetCurrentProcessId();
#else
    const unsigned long pid = static_cast<unsigned long>(getpid());
#endif
    return "voice_ring_" + std::to_string(pid) + "_" +
           std::to_string(seq.fetch_add(1, std::memory_order_relaxed)) + ".bin";
}

} // namespace

// A file of a fixed size with positioned reads and writes. The file goes
// away when it is closed, and when the process dies.
class RingFile {
public:
    RingFile() = default;
    ~RingFile() { close(); }
    RingFile(const RingFile&) = delete;
    RingFile& operator=(const RingFile&) = delete;

    bool open(const std::filesystem::path& path, uint64_t size) {
#ifdef _WIN32
        handle_ = CreateFileW(path.c_str(), GENERIC_READ | GENERIC_WRITE, 0, nullptr,
                              CREATE_ALWAYS,
                              FILE_ATTRIBUTE_NORMAL | FILE_FLAG_DELETE_ON_CLOSE, nullptr);
        if (handle_ == INVALID_HANDLE_VALUE) return false;
        LARGE_INTEGER end;
        end.QuadPart = static_cast<LONGLONG>(size);
        if (!SetFilePointerEx(handle_, end, nullptr, FILE_BEGIN) || !SetEndOfFile(handle_)) {
            close();
            return false;
        }
        return true;
#else
        fd_ = ::open(path.c_str(), O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
        if (fd_ < 0) return false;
        // The open descriptor keeps the data. No name is left to clean up.
        ::unlink(path.c_str());
        if (::ftruncate(fd_, static_cast<off_t>(size)) != 0) {
            close();
            return false;
        }
        return true;
#endif
    }

    bool write_at(uint64_t offset, const void* data, size_t size) {
#ifdef _WIN32
        OVERLAPPED at{};
        at.Offset = static_cast<DWORD>(offset & 0xFFFFFFFFu);
        at.OffsetHigh = static_cast<DWORD>(offset >> 32);
        DWORD done = 0;
        return WriteFile(handle_, data, static_cast<DWORD>(size), &done, &at) && done == size;
#else
        const auto* p = static_cast<const uint8_t*>(data);
        while (size > 0) {
            const ssize_t n = ::pwrite(fd_, p, size, static_cast<off_t>(offset));
            if (n <= 0) return false;
            p += n;
            offset += static_cast<uint64_t>(n);
            size -= static_cast<size_t>(n);
        }
        return true;
#endif
    }

    bool read_at(uint64_t offset, void* data, size_t size) {
#ifdef _WIN32
        OVERLAPPED at{};
        at.Offset = static_cast<DWORD>(offset & 0xFFFFFFFFu);
        at.OffsetHigh = static_cast<DWORD>(offset >> 32);
        DWORD done = 0;
        return ReadFile(handle_, data, static_cast<DWORD>(size), &done, &at) && done == size;
#else
        auto* p = static_cast<uint8_t*>(data);
        while (size > 0) {
            const ssize_t n = ::pread(fd_, p, size, static_cast<off_t>(offset));
            if (n <= 0) return false;
            p += n;
            offset += static_cast<uint64_t>(n);
            size -= static_cast<size_t>(n);
        }
        return true;
#endif
    }

    void close() {
#ifdef _WIN32
        if (handle_ != INVALID_HANDLE_VALUE) {
            CloseHandle(handle_);
            handle_ = INVALID_HANDLE_VALUE;
        }
#else
        if (fd_ >= 0) {
            ::close(fd_);
            fd_ = -1;
        }
#endif
    }

private:
#ifdef _WIN32
    HANDLE handle_ = INVALID_HANDLE_VALUE;
#else
    int fd_ = -1;
#endif
};

ClipBuffer::ClipBuffer(int sample_rate, int buffer_seconds, std::string dir)
    : sample_rate_(sample_rate)
    , frame_samples_(static_cast<size_t>(sample_rate) / 50)
    , slot_count_(static_cast<size_t>(buffer_seconds) * 50)
    , dir_(std::move(dir))
    , pending_(static_cast<size_t>(sample_rate) * kPendingSeconds)
    , frame_(static_cast<size_t>(sample_rate) / 50) {}

ClipBuffer::~ClipBuffer() {
    stop();
}

bool ClipBuffer::start() {
    // A second start (a rejoin) begins a new, empty buffer.
    if (active_.load(std::memory_order_acquire) || writer_.joinable()) stop();

    const std::filesystem::path dir = dir_.empty() ? default_dir() : std::filesystem::path(dir_);
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

    const std::filesystem::path path = dir / ring_file_name();
    auto file = std::make_unique<RingFile>();
    if (!file->open(path, ring_bytes())) {
        MELLO_LOG_ERROR("clip_buffer", "start: cannot make ring file %s", path.string().c_str());
        return false;
    }
    auto encoder = std::make_unique<OpusEnc>();
    if (!encoder->initialize(sample_rate_, 1, kRingBitrate, OpusApplication::Audio)) {
        MELLO_LOG_ERROR("clip_buffer", "start: Opus encoder init failed");
        return false;
    }

    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        file_ = std::move(file);
        encoder_ = std::move(encoder);
        frames_written_ = 0;
    }
    pending_.clear();
    dropped_.store(0, std::memory_order_relaxed);
    {
        std::lock_guard<std::mutex> lock(wake_mutex_);
        stop_writer_ = false;
    }
    active_.store(true, std::memory_order_release);
    writer_ = std::thread([this] { writer_loop(); });

    MELLO_LOG_INFO("clip_buffer", "started: %s (%zu frames, %.1fs, %llu bytes on disk)",
                   path.string().c_str(), slot_count_,
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
        file_.reset(); // closing deletes the ring file
        encoder_.reset();
        frames_written_ = 0;
    }
    pending_.clear();
    if (was_active) {
        MELLO_LOG_INFO("clip_buffer", "stopped (dropped %llu samples)",
                       static_cast<unsigned long long>(dropped_.load(std::memory_order_relaxed)));
    }
}

void ClipBuffer::write(const int16_t* samples, size_t count) {
    if (!active_.load(std::memory_order_relaxed)) return;
    const size_t queued = pending_.write(samples, count);
    if (queued < count) {
        dropped_.fetch_add(count - queued, std::memory_order_relaxed);
    }
}

void ClipBuffer::writer_loop() {
    for (;;) {
        {
            std::unique_lock<std::mutex> lock(wake_mutex_);
            wake_.wait_for(lock, kWriterPeriod, [this] { return stop_writer_; });
            if (stop_writer_) return;
        }
        std::lock_guard<std::mutex> lock(file_mutex_);
        drain_locked();
    }
}

void ClipBuffer::flush() {
    std::lock_guard<std::mutex> lock(file_mutex_);
    drain_locked();
}

void ClipBuffer::drain_locked() {
    if (!file_ || !encoder_ || !active_.load(std::memory_order_relaxed)) return;

    uint8_t slot[kSlotBytes];
    while (pending_.available() >= frame_samples_) {
        pending_.read(frame_.data(), frame_samples_);
        int len = encoder_->encode(frame_.data(), static_cast<int>(frame_samples_),
                                   slot + 2, static_cast<int>(kMaxPacketBytes));
        if (len < 0) {
            // An empty slot decodes as concealment. The ring keeps its time.
            MELLO_LOG_WARN("clip_buffer", "Opus encode failed (%d)", len);
            len = 0;
        }
        slot[0] = static_cast<uint8_t>(len & 0xFF);
        slot[1] = static_cast<uint8_t>((len >> 8) & 0xFF);
        const uint64_t offset = (frames_written_ % slot_count_) * kSlotBytes;
        if (!file_->write_at(offset, slot, kSlotBytes)) {
            // CLIPS.md 4.4: a full or failing disk turns the buffer off.
            MELLO_LOG_ERROR("clip_buffer", "ring file write failed, buffer off");
            active_.store(false, std::memory_order_release);
            return;
        }
        ++frames_written_;
    }
}

bool ClipBuffer::capture(float seconds, const std::string& output_path) {
    if (seconds <= 0.0f || output_path.empty()) {
        MELLO_LOG_WARN("clip_buffer", "capture: invalid params (seconds=%.1f)", seconds);
        return false;
    }

    const size_t requested = static_cast<size_t>(seconds * sample_rate_);
    const uint64_t want_frames = (requested + frame_samples_ - 1) / frame_samples_;

    // The packets of the window, and one before it to prime the decoder.
    std::vector<std::vector<uint8_t>> packets;
    bool has_primer = false;
    {
        std::lock_guard<std::mutex> lock(file_mutex_);
        if (!file_ || !active_.load(std::memory_order_relaxed)) {
            MELLO_LOG_WARN("clip_buffer", "capture: buffer not active");
            return false;
        }
        // Take the newest audio too, not only what the writer saw last.
        drain_locked();

        const uint64_t available = std::min<uint64_t>(frames_written_, slot_count_);
        const uint64_t take = std::min(want_frames, available);
        if (take == 0) {
            MELLO_LOG_WARN("clip_buffer", "capture: no audio in buffer");
            return false;
        }
        has_primer = take < available;
        const uint64_t first = frames_written_ - take - (has_primer ? 1 : 0);

        uint8_t slot[kSlotBytes];
        packets.reserve(static_cast<size_t>(frames_written_ - first));
        for (uint64_t i = first; i < frames_written_; ++i) {
            if (!file_->read_at((i % slot_count_) * kSlotBytes, slot, kSlotBytes)) {
                MELLO_LOG_ERROR("clip_buffer", "capture: ring file read failed");
                return false;
            }
            size_t len = static_cast<size_t>(slot[0]) | (static_cast<size_t>(slot[1]) << 8);
            if (len > kMaxPacketBytes) len = 0; // a bad slot: conceal it
            packets.emplace_back(slot + 2, slot + 2 + len);
        }
    }

    OpusDec decoder;
    if (!decoder.initialize(sample_rate_, 1)) {
        MELLO_LOG_ERROR("clip_buffer", "capture: Opus decoder init failed");
        return false;
    }
    std::vector<int16_t> pcm;
    pcm.reserve(packets.size() * frame_samples_);
    std::vector<int16_t> frame(frame_samples_);
    for (size_t i = 0; i < packets.size(); ++i) {
        const auto& p = packets[i];
        int n = p.empty()
            ? decoder.decode_plc(frame.data(), static_cast<int>(frame_samples_))
            : decoder.decode(p.data(), static_cast<int>(p.size()), frame.data(),
                             static_cast<int>(frame_samples_));
        if (n < 0) {
            std::fill(frame.begin(), frame.end(), static_cast<int16_t>(0));
            n = static_cast<int>(frame_samples_);
        }
        if (has_primer && i == 0) continue;
        pcm.insert(pcm.end(), frame.begin(), frame.begin() + n);
    }

    // The window ends now: keep the last `requested` samples.
    if (pcm.size() > requested) {
        pcm.erase(pcm.begin(), pcm.begin() + static_cast<std::ptrdiff_t>(pcm.size() - requested));
    }

    MELLO_LOG_INFO("clip_buffer", "capture: extracted %zu samples (%.1fs) from the ring file",
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
