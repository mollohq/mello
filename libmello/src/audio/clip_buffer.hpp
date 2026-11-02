#pragma once
#include "../replay/replay_audio_mix.hpp"
#include "../util/ring_buffer.hpp"
#include "../util/ring_file.hpp"
#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <deque>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <utility>
#include <vector>

namespace mello::audio {

class OpusEnc;
using util::RingFile;

// Rolling disk buffer of the voice for voice clips (F7, CLIPS.md section 4)
// and game clips (F8, plans/GAME-CLIPS.md 5.1).
//
// Two streams, each 20 ms Opus frames in a ring file of fixed slots on disk,
// the oldest frame overwritten:
// - the crew mix as the user heard it, from the render thread, with the time
//   it played (render time plus output latency) and m3llo's volume in the
//   Windows volume mixer (game clips Q20);
// - the local mic after the echo canceller, from the capture thread, with its
//   capture time (game clips Q24).
// Each producer hands its samples and their time to a small RAM queue (2 s).
// A writer thread encodes them into the ring files. The time of each frame
// travels with its samples and is stored in its slot.
//
// The ring files are deleted when the buffer stops. They are opened delete
// on close (Windows) or unlinked after open (POSIX), so a crash leaves none.
//
// Threads: write_crew() on the audio render thread and write_mic() on the
// audio capture thread (neither blocks on disk). start(), stop(), capture()
// and read_window() on any thread, one at a time.
class ClipBuffer {
public:
    // `dir` is the folder of the ring files. Empty: the OS temp folder, in
    // its `mello_clips` subfolder.
    explicit ClipBuffer(int sample_rate = 48000, int buffer_seconds = 60,
                        std::string dir = {});
    ~ClipBuffer();

    ClipBuffer(const ClipBuffer&) = delete;
    ClipBuffer& operator=(const ClipBuffer&) = delete;

    // The crew mix, mono. `heard_us`: the steady-clock time the first sample
    // plays on the speakers. `volume`: m3llo's session volume, 0 to 1.
    // Copies into the RAM queue only. A full queue drops the samples.
    void write_crew(const int16_t* samples, size_t count, uint64_t heard_us, float volume = 1.0f);
    // The local mic, mono. `capture_us`: the capture time of the first sample.
    void write_mic(const int16_t* samples, size_t count, uint64_t capture_us);

    // F7: the last `seconds` of both streams, mixed at their own times, as a
    // 16-bit mono WAV at `output_path`. The window ends at the newest crew
    // audio. Blocks until the file is written.
    bool capture(float seconds, const std::string& output_path);

    // F8: the frames of both streams that overlap [start_us, end_us),
    // decoded, each with its time, and the crew volume as gain points.
    struct VoiceWindow {
        std::vector<replay::TimedPcm> crew;
        std::vector<replay::TimedPcm> mic;
        std::vector<std::pair<uint64_t, float>> crew_volume;
    };
    bool read_window(uint64_t start_us, uint64_t end_us, VoiceWindow& out);

    // Encode what the RAM queues hold into the ring files now, on this
    // thread. The writer thread does the same every 20 ms.
    void flush();

    // Create the ring files and start the writer. Returns false, and stays
    // inactive, when the folder has less than kMinFreeBytes free or a file
    // cannot be made.
    bool start();
    // Stop the writer and delete the ring files.
    void stop();
    bool is_active() const { return active_.load(std::memory_order_relaxed); }

    // The size of the ring files in bytes, both streams.
    uint64_t ring_bytes() const { return 2 * static_cast<uint64_t>(slot_count_) * kSlotBytes; }
    // Samples the RAM queues dropped because the writer fell behind.
    uint64_t dropped_samples() const;

    // CLIPS.md 4.4: under this much free space the buffer does not start.
    static constexpr uint64_t kMinFreeBytes = 100ull * 1024 * 1024;
    // One slot: a 2-byte length, the 8-byte time of the frame, the volume as
    // a 16-bit fraction, then the Opus packet. Little-endian.
    static constexpr size_t kSlotBytes = 256;
    static constexpr size_t kSlotHeaderBytes = 12;
    static constexpr size_t kMaxPacketBytes = kSlotBytes - kSlotHeaderBytes;

private:
    // The time of the sample at `sample_index` of a stream, and the volume.
    struct Anchor {
        uint64_t sample_index;
        uint64_t time_us;
        float volume;
    };

    struct Stream {
        Stream(const char* name, size_t pending_samples);
        const char* name;

        // Producer to writer.
        util::RingBuffer<int16_t> pending;
        util::RingBuffer<Anchor> anchors;
        uint64_t samples_written = 0;  // producer thread
        std::atomic<uint64_t> dropped{0};

        // Writer side (file_mutex_).
        std::unique_ptr<RingFile> file;
        std::unique_ptr<OpusEnc> encoder;
        std::deque<Anchor> anchors_seen;
        uint64_t samples_read = 0;
        uint64_t frames_written = 0;
        std::vector<uint64_t> slot_time;  // the time of the frame in each slot
    };

    void writer_loop();
    // file_mutex_ held. Encodes every whole frame in the queue into the ring.
    void drain_locked(Stream& s);
    void write(Stream& s, const int16_t* samples, size_t count, uint64_t time_us, float volume);
    bool open_stream(Stream& s, const std::filesystem::path& dir, const char* prefix);
    // file_mutex_ held. The frames of `s` that overlap [start_us, end_us),
    // decoded, oldest first. `volume` gets a point at each volume change.
    bool read_frames(Stream& s, uint64_t start_us, uint64_t end_us,
                     std::vector<replay::TimedPcm>& out,
                     std::vector<std::pair<uint64_t, float>>* volume);
    bool write_wav(const std::string& path, const int16_t* data,
                   size_t sample_count, int sample_rate);

    const int sample_rate_;
    const size_t frame_samples_;   // 20 ms
    const size_t slot_count_;      // buffer_seconds of frames
    const std::string dir_;

    Stream crew_;
    Stream mic_;
    // The Opus encoder's lookahead: a decoded frame starts this much before
    // the time of its input frame (file_mutex_).
    uint64_t lookahead_us_ = 0;

    // Guards the files, the encoders and the writer-side counters. The
    // producer threads never take it.
    std::mutex file_mutex_;
    std::vector<int16_t> frame_;

    std::thread writer_;
    std::mutex wake_mutex_;
    std::condition_variable wake_;
    bool stop_writer_ = false;

    std::atomic<bool> active_{false};
};

} // namespace mello::audio
