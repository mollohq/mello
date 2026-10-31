#pragma once

// PipeWire plumbing shared by the Linux capture and playback backends.
// Linux only: CMake compiles the PipeWire files on Linux and nowhere else.

#if defined(__linux__)
#include <pipewire/pipewire.h>
#include <spa/node/io.h>
#include <spa/param/audio/format-utils.h>
#include <atomic>
#include <cstdint>
#include <functional>
#include <string>

namespace mello::audio::pw {

/// pw_init() once per process. Safe from any thread.
void ensure_init();

/// Sample rate of every libmello stream. PipeWire converts to and from the
/// device rate inside the stream (spec 10 §2: the 48 kHz contract).
constexpr uint32_t kRate = 48000;

/// The cycle length libmello asks for: 10 ms (`node.latency`). The graph may
/// run a longer cycle when another client needs one.
constexpr uint32_t kRequestedFrames = 480;

/// The default device name out of a `default.audio.*` metadata value, for
/// example `{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}`. Empty when
/// the value has no "name" string.
std::string parse_default_node_name(const char* json);

/// One pw_stream on its own pw_thread_loop.
///
/// Threads: open(), connect(), disconnect() and close() run on the caller's
/// thread and take the loop lock. The process callback runs on the loop
/// thread, with the loop lock held, so a disconnect() waits for a process
/// call that runs now. The stream is not RT_PROCESS: the callbacks do APM,
/// ONNX and NetEQ work, which must not run on PipeWire's real-time thread.
class Stream {
public:
    using ProcessFn = std::function<void(pw_stream*)>;

    Stream() = default;
    ~Stream();
    Stream(const Stream&) = delete;
    Stream& operator=(const Stream&) = delete;

    /// Create the loop and the stream object. This connects to the PipeWire
    /// daemon but makes no node yet. False when PipeWire is not reachable.
    bool open(const char* node_name, bool capture, ProcessFn process);

    /// Make the node: negotiate S16 at 48 kHz with `channels` channels, routed
    /// to `target` (a node.name; empty means the default device). Waits up to
    /// 3 s for the node to be ready. False on error or timeout.
    bool connect(uint32_t channels, const std::string& target);

    /// Remove the node. The stream object stays, so connect() works again.
    void disconnect();

    /// Stop the loop and free everything. Called by the destructor.
    void close();

    /// Frames the current graph cycle needs, in 48 kHz frames. From the
    /// graph position when the stream has it, else the requested 10 ms.
    /// Loop thread only (inside the process callback).
    uint32_t cycle_frames() const;

private:
    static void on_state_changed(void* data, enum pw_stream_state old,
                                 enum pw_stream_state state, const char* error);
    static void on_process(void* data);
    static void on_io_changed(void* data, uint32_t id, void* area, uint32_t size);

    pw_thread_loop* loop_ = nullptr;
    pw_stream* stream_ = nullptr;
    spa_hook listener_{};
    pw_stream_events events_{};
    ProcessFn process_;
    bool capture_ = false;
    bool connected_ = false;
    std::string name_;
    // Written and read on the loop thread, or with the loop lock held.
    pw_stream_state state_ = PW_STREAM_STATE_UNCONNECTED;
    spa_io_position* position_ = nullptr;
};

} // namespace mello::audio::pw
#endif
