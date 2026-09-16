#include "capture_hook.hpp"

#ifdef _WIN32

#include <windows.h>

#include <chrono>

#include "../util/log.hpp"
#include "hook_launcher.hpp"

using Microsoft::WRL::ComPtr;

namespace mello::video {
namespace {

constexpr const char* TAG = "video/hook";

// The hook has 4 s to load and report ready, the same deadline the injection
// helper works to (plan 3.4).
constexpr uint32_t kInjectTimeoutMs = 4000;

// How long to wait for the game's first present before the stream starts. A
// game at 60 fps presents every 17 ms; a game that is between levels can take
// longer, and then the window size is used instead.
constexpr uint32_t kFirstFrameTimeoutMs = 1000;

uint64_t now_us() {
    return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::microseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count());
}

int64_t qpc_now() {
    LARGE_INTEGER value{};
    QueryPerformanceCounter(&value);
    return value.QuadPart;
}

std::atomic<uint64_t>* as_atomic_u64(uint64_t* p) {
    return reinterpret_cast<std::atomic<uint64_t>*>(p);
}
std::atomic<uint32_t>* as_atomic_u32(uint32_t* p) {
    return reinterpret_cast<std::atomic<uint32_t>*>(p);
}

// A fatal error means the hook is loaded but cannot deliver. The ladder moves
// on. Errors that only describe one swap chain are not fatal.
bool error_is_fatal(uint32_t error) {
    switch (error) {
        case MELLO_HOOK_ERR_EXCEPTION:
        case MELLO_HOOK_ERR_DETOUR:
        case MELLO_HOOK_ERR_NO_OFFSETS:
        case MELLO_HOOK_ERR_SHARED_TEXTURE:
            return true;
        default:
            return false;
    }
}

const char* error_name(uint32_t error) {
    switch (error) {
        case MELLO_HOOK_OK:                 return "ok";
        case MELLO_HOOK_ERR_NO_DEVICE:      return "no D3D11 device on the swap chain";
        case MELLO_HOOK_ERR_SHARED_TEXTURE: return "shared texture failed";
        case MELLO_HOOK_ERR_FORMAT:         return "back buffer format cannot be shared";
        case MELLO_HOOK_ERR_COPY:           return "the copy failed";
        case MELLO_HOOK_ERR_EXCEPTION:      return "a detour faulted";
        case MELLO_HOOK_ERR_MULTISAMPLED:   return "multisampled back buffer";
        case MELLO_HOOK_ERR_NO_OFFSETS:     return "no present offsets";
        case MELLO_HOOK_ERR_DETOUR:         return "Detours refused the transaction";
        default:                            return "unknown";
    }
}

// The window the game presents to, used only for its size while no frame has
// arrived yet.
bool window_client_size(uint32_t pid, uint32_t* width, uint32_t* height) {
    struct Search {
        uint32_t pid;
        LONG     area = 0;
        uint32_t width = 0;
        uint32_t height = 0;
    } search{pid};

    EnumWindows(
        [](HWND window, LPARAM param) -> BOOL {
            auto* s = reinterpret_cast<Search*>(param);
            DWORD owner = 0;
            GetWindowThreadProcessId(window, &owner);
            if (owner != s->pid || !IsWindowVisible(window)) return TRUE;
            RECT rect{};
            if (!GetClientRect(window, &rect)) return TRUE;
            const LONG area = (rect.right - rect.left) * (rect.bottom - rect.top);
            if (area <= s->area) return TRUE;
            s->area = area;
            s->width = static_cast<uint32_t>(rect.right - rect.left);
            s->height = static_cast<uint32_t>(rect.bottom - rect.top);
            return TRUE;
        },
        reinterpret_cast<LPARAM>(&search));

    if (search.width == 0 || search.height == 0) return false;
    *width = search.width;
    *height = search.height;
    return true;
}

} // namespace

int HookCapture::process_bitness(uint32_t pid) {
    const HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
    if (!process) return 64;

    USHORT process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    USHORT native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    int bits = 64;
    if (IsWow64Process2(process, &process_machine, &native_machine)) {
        // A process that runs under WOW64 reports its own 32-bit machine here.
        bits = process_machine == IMAGE_FILE_MACHINE_UNKNOWN ? 64 : 32;
    }
    CloseHandle(process);
    return bits;
}

HookCapture::~HookCapture() {
    stop();
    release_shared_block();
}

bool HookCapture::create_shared_block(uint32_t pid) {
    char name[64];

    mello_hook::object_name(name, sizeof(name), MELLO_HOOK_NAME_INFO, pid);
    // CreateFileMapping opens the block a still-loaded hook from an earlier
    // stream is holding, which is how a second stream reuses that hook.
    mapping_ = CreateFileMappingA(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE, 0,
                                  sizeof(MelloHookInfo), name);
    if (!mapping_) {
        MELLO_LOG_ERROR(TAG, "shared block for pid=%u failed: %lu", pid, GetLastError());
        return false;
    }
    const bool existed = GetLastError() == ERROR_ALREADY_EXISTS;

    info_ = static_cast<MelloHookInfo*>(
        MapViewOfFile(mapping_, FILE_MAP_ALL_ACCESS, 0, 0, sizeof(MelloHookInfo)));
    if (!info_) {
        MELLO_LOG_ERROR(TAG, "mapping the block for pid=%u failed: %lu", pid, GetLastError());
        return false;
    }
    if (!existed) {
        ZeroMemory(info_, sizeof(MelloHookInfo));
    }
    info_->protocol_version = MELLO_HOOK_PROTOCOL_VERSION;
    info_->struct_size = sizeof(MelloHookInfo);
    info_->client_pid = GetCurrentProcessId();
    info_->capture_enabled = 0;
    beat();

    mello_hook::object_name(name, sizeof(name), MELLO_HOOK_NAME_READY, pid);
    ready_event_ = CreateEventA(nullptr, TRUE, FALSE, name);
    mello_hook::object_name(name, sizeof(name), MELLO_HOOK_NAME_FRAME, pid);
    frame_event_ = CreateEventA(nullptr, FALSE, FALSE, name);
    mello_hook::object_name(name, sizeof(name), MELLO_HOOK_NAME_STOP, pid);
    stop_event_ = CreateEventA(nullptr, TRUE, FALSE, name);

    if (!ready_event_ || !frame_event_ || !stop_event_) {
        MELLO_LOG_ERROR(TAG, "hook events for pid=%u failed: %lu", pid, GetLastError());
        return false;
    }
    // Both events carry a meaning for this stream, not for the last one. The
    // stop event is still set from the previous teardown, and the ready event
    // is still set from the previous injection. A hook that is already loaded
    // raises ready again within half a second.
    ResetEvent(stop_event_);
    ResetEvent(ready_event_);
    return true;
}

void HookCapture::release_shared_block() {
    if (info_) {
        UnmapViewOfFile(info_);
        info_ = nullptr;
    }
    HANDLE* handles[] = {&mapping_, &ready_event_, &frame_event_, &stop_event_};
    for (HANDLE* handle : handles) {
        if (*handle) {
            CloseHandle(*handle);
            *handle = nullptr;
        }
    }
}

void HookCapture::beat() {
    if (!info_) return;
    as_atomic_u64(&info_->heartbeat_qpc)
        ->store(static_cast<uint64_t>(qpc_now()), std::memory_order_relaxed);
}

bool HookCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    if (desc.mode != CaptureMode::Process) {
        MELLO_LOG_ERROR(TAG, "the hook captures a process, nothing else");
        return false;
    }
    device_ = device.d3d11();
    if (!device_) return false;
    device_->GetImmediateContext(&context_);

    LARGE_INTEGER frequency{};
    QueryPerformanceFrequency(&frequency);
    qpc_frequency_ = frequency.QuadPart;

    pid_ = desc.pid;
    bits_ = process_bitness(pid_);

    const hook::Offsets& offsets = hook::offsets_for(bits_);
    if (!offsets.valid) {
        MELLO_LOG_WARN(TAG, "no present offsets for %d-bit games; the hook cannot start", bits_);
        return false;
    }

    if (!create_shared_block(pid_)) {
        release_shared_block();
        return false;
    }

    info_->offsets_valid = 1;
    info_->off_dxgi_present = offsets.dxgi_present;
    info_->off_dxgi_present1 = offsets.dxgi_present1;
    info_->off_dxgi_resize_buffers = offsets.dxgi_resize_buffers;
    // Capture is on from the start: the hook publishes the frame size with its
    // first frame, and the stream needs that size to size its encoder.
    as_atomic_u32(&info_->capture_enabled)->store(1, std::memory_order_relaxed);
    beat();

    const hook::InjectResult injected = hook::inject(pid_, bits_, kInjectTimeoutMs);
    if (injected != hook::InjectResult::Ready) {
        MELLO_LOG_WARN(TAG, "hook did not load into pid=%u (%d-bit): %s", pid_, bits_,
                       hook::inject_result_name(injected));
        release_shared_block();
        return false;
    }

    const uint32_t error = as_atomic_u32(&info_->last_error)->load(std::memory_order_relaxed);
    if (error_is_fatal(error)) {
        MELLO_LOG_WARN(TAG, "hook loaded into pid=%u but cannot capture: %s", pid_,
                       error_name(error));
        release_shared_block();
        return false;
    }

    if (!wait_for_first_frame(kFirstFrameTimeoutMs)) {
        // The game has not presented yet. Start the stream at the window size
        // and let the first frame correct it.
        if (!window_client_size(pid_, &width_, &height_)) {
            MELLO_LOG_WARN(TAG, "no frame and no window size for pid=%u", pid_);
            release_shared_block();
            return false;
        }
        MELLO_LOG_INFO(TAG, "hook is in pid=%u; no frame yet, starting at the window size %ux%u",
                       pid_, width_, height_);
        return true;
    }

    MELLO_LOG_INFO(TAG, "hook is in pid=%u (%d-bit), capturing %ux%u fmt=%u", pid_, bits_, width_,
                   height_, info_->dxgi_format);
    return true;
}

bool HookCapture::wait_for_first_frame(uint32_t timeout_ms) {
    const uint64_t deadline = now_us() + static_cast<uint64_t>(timeout_ms) * 1000;
    while (now_us() < deadline) {
        beat();
        if (as_atomic_u64(&info_->frame_index)->load(std::memory_order_acquire) > 0) {
            width_ = info_->width;
            height_ = info_->height;
            return width_ > 0 && height_ > 0;
        }
        WaitForSingleObject(frame_event_, 50);
    }
    return false;
}

// Opens the textures the hook published. Called when the generation changes:
// at the first frame, and after a resize.
bool HookCapture::refresh_textures() {
    const uint32_t generation =
        as_atomic_u32(&info_->texture_generation)->load(std::memory_order_acquire);
    if (generation == texture_generation_ && shared_[0]) return true;

    for (auto& texture : shared_) texture.Reset();
    copy_.Reset();

    for (uint32_t i = 0; i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
        const HANDLE handle =
            reinterpret_cast<HANDLE>(static_cast<uintptr_t>(info_->shared_handles[i]));
        if (!handle) return false;
        // A legacy shared handle opens straight on our device, whatever the
        // bitness of the game that made it.
        const HRESULT hr = device_->OpenSharedResource(handle, __uuidof(ID3D11Texture2D),
                                                       reinterpret_cast<void**>(
                                                           shared_[i].GetAddressOf()));
        if (FAILED(hr) || !shared_[i]) {
            MELLO_LOG_ERROR(TAG,
                            "cannot open the hook's texture %u (handle=%08x): hr=0x%08lx. The "
                            "game is probably on another GPU.",
                            i, info_->shared_handles[i], static_cast<unsigned long>(hr));
            failed_.store(true, std::memory_order_relaxed);
            return false;
        }
    }

    D3D11_TEXTURE2D_DESC desc{};
    shared_[0]->GetDesc(&desc);
    desc.Usage = D3D11_USAGE_DEFAULT;
    // The preprocessor turns this texture into a video processor input view,
    // and that refuses a texture bound only as a shader resource
    // (CreateVideoProcessorInputView, hr=0x80070057). The other backends hand
    // over textures with both flags, so this one does too.
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
    desc.MiscFlags = 0;
    desc.CPUAccessFlags = 0;
    if (FAILED(device_->CreateTexture2D(&desc, nullptr, copy_.GetAddressOf()))) {
        MELLO_LOG_ERROR(TAG, "cannot create the frame texture %ux%u", desc.Width, desc.Height);
        failed_.store(true, std::memory_order_relaxed);
        return false;
    }

    width_ = desc.Width;
    height_ = desc.Height;
    texture_generation_ = generation;
    MELLO_LOG_INFO(TAG, "hook frames are %ux%u fmt=%u (generation %u)", width_, height_,
                   static_cast<unsigned>(desc.Format), generation);
    return true;
}

bool HookCapture::start(uint32_t target_fps, FrameCallback callback) {
    if (!info_) return false;
    if (running_.load()) return true;

    target_fps_ = target_fps == 0 ? 60 : target_fps;
    callback_ = std::move(callback);
    running_.store(true);

    std::promise<void> exited;
    exited_future_ = exited.get_future();
    thread_ = std::thread([this, exited = std::move(exited)]() mutable {
        capture_thread();
        exited.set_value();
    });
    return true;
}

void HookCapture::capture_thread() {
    MELLO_LOG_INFO(TAG, "hook capture thread started for pid=%u", pid_);
    const uint64_t interval_us = 1'000'000ull / target_fps_;

    while (running_.load()) {
        beat();
        if (WaitForSingleObject(frame_event_, 100) != WAIT_OBJECT_0) {
            // No frame. A game that presents nothing is quiet, not broken: the
            // ladder rule for that lives in ProcessCapture.
            continue;
        }

        const uint64_t index = as_atomic_u64(&info_->frame_index)->load(std::memory_order_acquire);
        if (index == last_frame_index_) continue;
        last_frame_index_ = index;

        const uint32_t error = as_atomic_u32(&info_->last_error)->load(std::memory_order_relaxed);
        if (error_is_fatal(error)) {
            MELLO_LOG_ERROR(TAG, "the hook stopped capturing in pid=%u: %s", pid_,
                            error_name(error));
            failed_.store(true, std::memory_order_relaxed);
            break;
        }

        if (!refresh_textures()) break;

        // Deliver at the target rate. The game presents at its own rate, which
        // can be several times the stream's.
        const uint64_t now = now_us();
        if (last_delivered_us_ != 0 && now - last_delivered_us_ + 500 < interval_us) continue;
        last_delivered_us_ = now;

        const uint32_t slot = info_->texture_index % MELLO_HOOK_TEXTURE_COUNT;
        if (!shared_[slot] || !copy_) continue;
        // The copy takes the frame away from the hook's pair, so the game can
        // keep presenting into them while the pipeline reads this one.
        context_->CopyResource(copy_.Get(), shared_[slot].Get());

        if (delay_hist_ && qpc_frequency_ > 0) {
            const int64_t present_qpc = static_cast<int64_t>(info_->frame_qpc);
            const double delay_ms =
                static_cast<double>(qpc_now() - present_qpc) * 1000.0 /
                static_cast<double>(qpc_frequency_);
            delay_hist_->record_ms(delay_ms);
        }

        if (callback_) callback_(copy_.Get(), now);
    }

    MELLO_LOG_INFO(TAG, "hook capture thread for pid=%u stopped", pid_);
}

void HookCapture::stop() {
    if (!running_.exchange(false)) {
        if (info_) as_atomic_u32(&info_->capture_enabled)->store(0, std::memory_order_relaxed);
        return;
    }

    // Tell the hook first: it stops copying on the game's present path as soon
    // as it sees this, and the game pays nothing more.
    if (info_) as_atomic_u32(&info_->capture_enabled)->store(0, std::memory_order_relaxed);
    if (stop_event_) SetEvent(stop_event_);

    if (!thread_.joinable()) return;
    // This thread only waits on an event with a timeout, so it always comes
    // back. The bound is here because a capture thread that does not come back
    // must never hold the pipeline, whatever the reason (2026-09-15).
    if (exited_future_.valid() &&
        exited_future_.wait_for(std::chrono::seconds(2)) == std::future_status::ready) {
        thread_.join();
        return;
    }
    detached_.store(true, std::memory_order_relaxed);
    thread_.detach();
    MELLO_LOG_ERROR(TAG, "hook capture thread for pid=%u did not stop; it is detached", pid_);
}

} // namespace mello::video

#endif // _WIN32
