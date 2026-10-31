#pragma once
#include "../util/ring_buffer.hpp"
#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

namespace mello::audio {

class OpusEnc;
class RingFile;

// Rolling disk buffer for voice clip capture (CLIPS.md section 4).
//
// The render thread hands the mixed output to a small RAM queue (2 s). A
// writer thread encodes it as 20 ms Opus frames into a ring file of fixed
// slots on disk, the oldest frame overwritten. On capture, the last N seconds
// are decoded and written as a 16-bit mono WAV.
//
// The ring file is deleted when the buffer stops. It is opened delete on
// close (Windows) or unlinked after open (POSIX), so a crash leaves no file.
//
// Threads: write() on the audio render thread (never blocks on disk).
// start(), stop() and capture() on any thread, one at a time.
class ClipBuffer {
public:
    // `dir` is the folder of the ring file. Empty: the OS temp folder, in
    // its `mello_clips` subfolder.
    explicit ClipBuffer(int sample_rate = 48000, int buffer_seconds = 60,
                        std::string dir = {});
    ~ClipBuffer();

    ClipBuffer(const ClipBuffer&) = delete;
    ClipBuffer& operator=(const ClipBuffer&) = delete;

    // Append mixed audio samples. Called from the playback render path: it
    // only copies into the RAM queue. A full queue drops the new samples.
    void write(const int16_t* samples, size_t count);

    // Extract the last `seconds` of audio and save as WAV to `output_path`.
    // Blocks until file is written. Returns true on success.
    bool capture(float seconds, const std::string& output_path);

    // Encode what the RAM queue holds into the ring file now, on this thread.
    // The writer thread does the same every 20 ms.
    void flush();

    // Create the ring file and start the writer. Returns false, and stays
    // inactive, when the folder has less than kMinFreeBytes free or the file
    // cannot be made.
    bool start();
    // Stop the writer and delete the ring file.
    void stop();
    bool is_active() const { return active_.load(std::memory_order_relaxed); }

    // The size of the ring file in bytes.
    uint64_t ring_bytes() const { return static_cast<uint64_t>(slot_count_) * kSlotBytes; }
    // Samples the RAM queue dropped because the writer fell behind.
    uint64_t dropped_samples() const { return dropped_.load(std::memory_order_relaxed); }

    // CLIPS.md 4.4: under this much free space the buffer does not start.
    static constexpr uint64_t kMinFreeBytes = 100ull * 1024 * 1024;
    // One slot: a 2-byte little-endian length, then the Opus packet.
    static constexpr size_t kSlotBytes = 256;
    static constexpr size_t kMaxPacketBytes = kSlotBytes - 2;

private:
    void writer_loop();
    // file_mutex_ held. Encodes every whole frame in the queue into the ring.
    void drain_locked();
    bool write_wav(const std::string& path, const int16_t* data,
                   size_t sample_count, int sample_rate);

    const int sample_rate_;
    const size_t frame_samples_;   // 20 ms
    const size_t slot_count_;      // buffer_seconds of frames
    const std::string dir_;

    // Render thread to writer: the RAM queue (2 s).
    util::RingBuffer<int16_t> pending_;
    std::atomic<uint64_t> dropped_{0};

    // Guards the file, the encoder and frames_written_. The render thread
    // never takes it.
    std::mutex file_mutex_;
    std::unique_ptr<RingFile> file_;
    std::unique_ptr<OpusEnc> encoder_;
    uint64_t frames_written_ = 0;
    std::vector<int16_t> frame_;

    std::thread writer_;
    std::mutex wake_mutex_;
    std::condition_variable wake_;
    bool stop_writer_ = false;

    std::atomic<bool> active_{false};
};

} // namespace mello::audio
