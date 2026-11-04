// The game clip replay on Windows (replay_frames.hpp): WGC window capture,
// the D3D11 preprocessor into the replay's NV12 ring, and a staging texture
// for the poster.
#ifdef _WIN32
#include "replay_frames.hpp"
#include "replay_mux.hpp"
#include "../util/log.hpp"
#include "../video/capture_process.hpp"
#include "../video/graphics_device.hpp"
#include "../video/video_preprocessor.hpp"

#include <d3d11.h>
#include <wrl/client.h>

#include <condition_variable>
#include <mutex>

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/frames";

void set_error(std::string* error, const char* text) {
    MELLO_LOG_WARN(TAG, "%s", text);
    if (error) *error = text;
}

class WinReplayFrames final : public ReplayFrames {
public:
    bool init(const video::CaptureManager& manager, uint32_t enc_w, uint32_t enc_h, std::string* error) {
        // On the manager's device: the capture's textures are on it (zero
        // copy, 14-VIDEO-PIPELINE.md 2.2).
        preprocessor_ = std::make_unique<video::VideoPreprocessor>();
        if (!preprocessor_->initialize(manager.device(), manager.width(), manager.height(), enc_w, enc_h)) {
            set_error(error, "no video processor");
            preprocessor_.reset();
            return false;
        }
        return true;
    }

    void* take(const video::CapturedFrame& frame) override {
        // An NV12 slot of the preprocessor's ring: the encode queue is shorter
        // than the ring, so a slot is not reused while it waits.
        return preprocessor_->convert(frame.texture, nullptr).texture;
    }
    void release(void*) override {}
    bool encode(video::Encoder& encoder, void* frame, uint64_t, video::EncodedPacket& out) override {
        return encoder.encode(static_cast<ID3D11Texture2D*>(frame), out);
    }

    void keep_poster(const video::CapturedFrame& frame) override {
        ID3D11Texture2D* texture = frame.texture;
        if (!texture) return;
        D3D11_TEXTURE2D_DESC desc{};
        texture->GetDesc(&desc);
        if (desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM && desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB) {
            return;  // HDR or another format: the poster comes from the MP4
        }
        Microsoft::WRL::ComPtr<ID3D11Device> device;
        texture->GetDevice(&device);
        D3D11_TEXTURE2D_DESC staging_desc{};
        staging_desc.Width = desc.Width;
        staging_desc.Height = desc.Height;
        staging_desc.MipLevels = 1;
        staging_desc.ArraySize = 1;
        staging_desc.Format = desc.Format;
        staging_desc.SampleDesc.Count = 1;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
        if (FAILED(device->CreateTexture2D(&staging_desc, nullptr, &staging))) return;
        Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
        device->GetImmediateContext(&context);
        // A submit, no wait (12-STREAMING.md 3.7, F6). The map on the capture's caller waits.
        context->CopyResource(staging.Get(), texture);
        {
            std::lock_guard<std::mutex> lock(poster_mutex_);
            poster_staging_ = staging;
        }
        poster_cv_.notify_all();
    }

    bool write_poster(const std::filesystem::path& jpeg, std::chrono::milliseconds wait) override {
        Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
        {
            std::unique_lock<std::mutex> lock(poster_mutex_);
            if (!poster_cv_.wait_for(lock, wait, [this] { return poster_staging_ != nullptr; })) return false;
            staging = std::move(poster_staging_);
        }
        D3D11_TEXTURE2D_DESC desc{};
        staging->GetDesc(&desc);
        Microsoft::WRL::ComPtr<ID3D11Device> device;
        staging->GetDevice(&device);
        Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
        device->GetImmediateContext(&context);
        D3D11_MAPPED_SUBRESOURCE mapped{};
        if (FAILED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) return false;
        const bool ok = write_poster_jpeg(static_cast<const uint8_t*>(mapped.pData), desc.Width, desc.Height,
                                          mapped.RowPitch, jpeg);
        context->Unmap(staging.Get(), 0);
        return ok;
    }

    void drop_poster() override {
        std::lock_guard<std::mutex> lock(poster_mutex_);
        poster_staging_.Reset();
    }

private:
    std::unique_ptr<video::VideoPreprocessor> preprocessor_;
    std::mutex poster_mutex_;
    std::condition_variable poster_cv_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> poster_staging_;  // poster_mutex_
};

std::shared_ptr<video::CaptureManager> make_on_new_device(const video::CaptureSourceDesc& desc, std::string* error) {
    video::GraphicsDevice device = video::create_d3d11_device();
    if (!device.handle) {
        set_error(error, "no graphics device");
        return nullptr;
    }
    auto manager = video::CaptureManager::create_from(device, video::create_capture_source(desc), desc);
    if (!manager) set_error(error, "the game window cannot be captured");
    return manager;
}

}  // namespace

std::unique_ptr<ReplayFrames> make_replay_frames(const video::CaptureManager& manager, uint32_t enc_w,
                                                 uint32_t enc_h, std::string* error) {
    auto frames = std::make_unique<WinReplayFrames>();
    if (!frames->init(manager, enc_w, enc_h, error)) return nullptr;
    return frames;
}

uint32_t window_owner_pid(void* window) {
    DWORD pid = 0;
    if (window) GetWindowThreadProcessId(static_cast<HWND>(window), &pid);
    return static_cast<uint32_t>(pid);
}

void* game_main_window(uint32_t pid) { return pid ? video::find_main_window(pid) : nullptr; }

std::shared_ptr<video::CaptureManager> make_window_capture(void* window, uint32_t, std::string* error) {
    // WGC on the game window, and nothing else. No resolve_capture_target: it
    // would turn a window that cannot carry a stream into its process and
    // run the whole ladder, monitor capture included (CLIPS.md 15.3).
    video::CaptureSourceDesc desc{};
    desc.mode = video::CaptureMode::Window;
    desc.hwnd = window;
    return make_on_new_device(desc, error);
}

std::shared_ptr<video::CaptureManager> make_source_capture(const video::CaptureSourceDesc& desc,
                                                           std::string* error) {
    return make_on_new_device(desc, error);
}

}  // namespace mello::replay
#endif
