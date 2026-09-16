#pragma once
#include "capture_source.hpp"

#ifdef _WIN32

#include <wrl/client.h>

#include <atomic>
#include <future>
#include <mutex>
#include <thread>

#include "mello_hook_protocol.h"

namespace mello::video {

/// Capture through the m3llo game capture hook.
///
/// The hook DLL runs inside the game and copies each presented back buffer into
/// a shared texture. This class is the other end: it creates the shared block,
/// injects the hook, and turns the frames into the same callbacks every other
/// capture backend delivers.
///
/// It is the only method that can see an exclusive-fullscreen game, and the
/// only one that needs permission: `ProcessCapture` asks the hook policy before
/// it builds one (plan 3.6).
///
/// Threads:
///  - `initialize`, `start` and `stop` run on the pipeline thread.
///  - `capture_thread` waits for frames and delivers them. It also writes the
///    heartbeat that tells the hook the client is still alive.
class HookCapture : public CaptureSource {
public:
    ~HookCapture() override;

    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width() const override { return width_; }
    uint32_t height() const override { return height_; }
    const char* backend_name() const override { return "Hook"; }

    bool failed() const override { return failed_.load(std::memory_order_relaxed); }
    bool waiting_for_the_game() const override;
    // The frame is taken inside the game, so a minimized game still streams.
    bool captures_while_minimized() const override { return true; }
    bool stop_timed_out() const override { return detached_.load(std::memory_order_relaxed); }
    void set_present_delay_histogram(PresentDelayHistogram* hist) override { delay_hist_ = hist; }

    /// Bitness of the target process, 32 or 64. The helpers and the hook must
    /// match it.
    static int process_bitness(uint32_t pid);

private:
    bool create_shared_block(uint32_t pid);
    void release_shared_block();
    bool wait_for_first_frame(uint32_t timeout_ms);
    void capture_thread();
    bool refresh_textures();
    /// Opens the block a Direct3D 9 game writes its frames into, and builds the
    /// texture those frames are uploaded to.
    bool refresh_memory_frames();
    /// Copies one frame from that block into the texture the pipeline reads.
    bool upload_memory_frame(uint32_t slot);
    void beat();

    Microsoft::WRL::ComPtr<ID3D11Device>        device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>     shared_[MELLO_HOOK_TEXTURE_COUNT];
    Microsoft::WRL::ComPtr<ID3D11Texture2D>     copy_;

    HANDLE         mapping_     = nullptr;
    MelloHookInfo* info_        = nullptr;
    // Frames through memory, for Direct3D 9 games. Empty for every other API.
    HANDLE         frames_mapping_ = nullptr;
    const uint8_t* frames_        = nullptr;
    uint32_t       frame_bytes_   = 0;
    uint32_t       frame_pitch_   = 0;
    HANDLE         ready_event_ = nullptr;
    HANDLE         frame_event_ = nullptr;
    HANDLE         stop_event_  = nullptr;

    uint32_t pid_ = 0;
    int      bits_ = 64;
    uint32_t width_ = 0;
    uint32_t height_ = 0;
    uint32_t texture_generation_ = 0;
    uint64_t last_frame_index_ = 0;
    int64_t  qpc_frequency_ = 0;

    uint32_t target_fps_ = 60;
    uint64_t last_delivered_us_ = 0;

    std::thread       thread_;
    std::atomic<bool> running_{false};
    std::atomic<bool> failed_{false};
    std::atomic<bool> detached_{false};
    std::future<void> exited_future_;
    FrameCallback     callback_;
    PresentDelayHistogram* delay_hist_ = nullptr;
};

} // namespace mello::video

#endif // _WIN32
