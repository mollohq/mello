#include "capture_hook.hpp"

#ifdef _WIN32

#include <windows.h>

#include <chrono>

#include "../util/log.hpp"
#include "capture_process.hpp"
#include "cursor.hpp"
#include "frame_cadence.hpp"
#include "hook_launcher.hpp"

using Microsoft::WRL::ComPtr;

namespace mello::video {
namespace {

constexpr const char* TAG = "video/hook";

// The hook has 4 s to load and report ready, the same deadline the injection
// helper works to (streaming-reliability plan section 7.3).
constexpr uint32_t kInjectTimeoutMs = 4000;

// A game whose present count has not moved for this long has stopped drawing.
// One second is long enough to cover a slow frame on a loaded machine and short
// enough that viewers are told about an alt-tab quickly. The pause card has its
// own debounce on top.
constexpr uint64_t kGameQuietUs = 1'000'000;

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

// Both rules live in the protocol header, so the hook and the client cannot
// disagree about what an error means.
bool error_is_fatal(uint32_t error) { return mello_hook_error_is_fatal(error) != 0; }
bool error_is_transient(uint32_t error) { return mello_hook_error_is_transient(error) != 0; }

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
        case MELLO_HOOK_ERR_DEVICE_LOST:    return "the game's graphics device is lost";
        default:                            return "unknown";
    }
}

// The size of the game's largest window, used while no frame has arrived yet.
//
// It reads the restored size, not the current one, and it does not ask whether
// the window is visible. A game that is still starting, or one that has just
// taken exclusive fullscreen, has a window that answers neither question the
// way a normal window does: measured against Unigine Heaven on 2026-09-16,
// where its only window was a 0x0 proxy and the hook refused to start because
// of it, and the stream fell back to capturing the whole desktop.
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
            if (owner != s->pid) return TRUE;

            WINDOWPLACEMENT placement{};
            placement.length = sizeof(placement);
            if (!GetWindowPlacement(window, &placement)) return TRUE;
            const RECT& restored = placement.rcNormalPosition;
            const LONG width = restored.right - restored.left;
            const LONG height = restored.bottom - restored.top;
            if (width <= 0 || height <= 0) return TRUE;

            const LONG area = width * height;
            if (area <= s->area) return TRUE;
            s->area = area;
            s->width = static_cast<uint32_t>(width);
            s->height = static_cast<uint32_t>(height);
            return TRUE;
        },
        reinterpret_cast<LPARAM>(&search));

    if (search.width < kMinEncodeWidth || search.height < kMinEncodeHeight) return false;
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
    // The error field describes this stream. A hook that is still loaded from
    // an earlier stream leaves its last one behind, and reading that would
    // refuse a hook that works.
    info_->last_error = MELLO_HOOK_OK;
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
    if (frames_) {
        UnmapViewOfFile(const_cast<uint8_t*>(frames_));
        frames_ = nullptr;
    }
    if (frames_mapping_) {
        CloseHandle(frames_mapping_);
        frames_mapping_ = nullptr;
    }
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
    // The hook shares Direct3D 9 back buffers only with this adapter. A
    // shared handle is valid nowhere else, so the hook must know before it
    // chooses the GPU path or the memory path.
    info_->client_adapter_luid = device.adapter_luid;

    info_->offsets_valid = 1;
    info_->off_dxgi_present = offsets.dxgi_present;
    info_->off_dxgi_present1 = offsets.dxgi_present1;
    info_->off_dxgi_resize_buffers = offsets.dxgi_resize_buffers;
    info_->off_d3d9_present = offsets.d3d9_present;
    info_->off_d3d9_present_ex = offsets.d3d9_present_ex;
    info_->off_d3d9_swapchain_present = offsets.d3d9_swapchain_present;
    info_->off_d3d9_reset = offsets.d3d9_reset;
    info_->off_d3d9_reset_ex = offsets.d3d9_reset_ex;
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
        uint32_t window_w = 0, window_h = 0;
        if (!window_client_size(pid_, &window_w, &window_h)) {
            MELLO_LOG_WARN(TAG, "no frame and no window size for pid=%u", pid_);
            release_shared_block();
            return false;
        }
        width_.store(window_w);
        height_.store(window_h);
        MELLO_LOG_INFO(TAG, "hook is in pid=%u; no frame yet, starting at the window size %ux%u",
                       pid_, window_w, window_h);
        return true;
    }

    MELLO_LOG_INFO(TAG, "hook is in pid=%u (%d-bit), capturing %ux%u fmt=%u", pid_, bits_,
                   width_.load(), height_.load(), info_->dxgi_format);
    return true;
}

HookCapture::Counters HookCapture::counters() const {
    Counters out;
    if (!info_) return out;
    auto* info = const_cast<MelloHookInfo*>(info_);
    out.presents_seen = as_atomic_u64(&info->presents_seen)->load(std::memory_order_relaxed);
    out.frames_copied = as_atomic_u64(&info->frames_copied)->load(std::memory_order_relaxed);
    out.frames_dropped = as_atomic_u64(&info->frames_dropped)->load(std::memory_order_relaxed);
    return out;
}

uint32_t HookCapture::texture_generation() const {
    if (!info_) return 0;
    return as_atomic_u32(const_cast<uint32_t*>(&info_->texture_generation))
        ->load(std::memory_order_relaxed);
}

bool HookCapture::waiting_for_the_game() const {
    if (!info_) return false;

    // A device that is lost fails every call until the game resets it. The
    // game is fine, the hook is fine, and the wait is short: Direct3D 9 does
    // this while a display mode changes.
    if (error_is_transient(
            as_atomic_u32(const_cast<uint32_t*>(&info_->last_error))->load(std::memory_order_relaxed))) {
        return true;
    }

    const uint64_t presents =
        as_atomic_u64(const_cast<uint64_t*>(&info_->presents_seen))->load(std::memory_order_relaxed);
    if (presents == 0) return true;  // the game has drawn nothing at all yet

    // The count stopped moving: the game stopped drawing. A fullscreen game
    // does that the moment somebody alt-tabs away from it.
    const uint64_t changed_at = last_presents_change_us_.load(std::memory_order_relaxed);
    if (changed_at == 0) return false;
    const uint64_t now = now_us();
    return now > changed_at && now - changed_at > kGameQuietUs;
}

bool HookCapture::wait_for_first_frame(uint32_t timeout_ms) {
    const uint64_t deadline = now_us() + static_cast<uint64_t>(timeout_ms) * 1000;
    while (now_us() < deadline) {
        beat();
        if (as_atomic_u64(&info_->frame_index)->load(std::memory_order_acquire) > 0) {
            width_ = info_->width;
            height_ = info_->height;
            return width_.load() > 0 && height_.load() > 0;
        }
        WaitForSingleObject(frame_event_, 50);
    }
    return false;
}

// Opens the block a Direct3D 9 game writes its frames into when it cannot
// share textures: a plain D3D9 device, or a D3D9Ex device on another adapter
// than the client's. Those frames come through memory (streaming-reliability
// plan section 7.4).
bool HookCapture::refresh_memory_frames() {
    const uint32_t bytes = info_->cpu_frame_bytes;
    const uint32_t pitch = info_->cpu_pitch;
    if (bytes == 0 || pitch == 0) return false;

    if (frames_) {
        UnmapViewOfFile(const_cast<uint8_t*>(frames_));
        frames_ = nullptr;
    }
    if (frames_mapping_) {
        CloseHandle(frames_mapping_);
        frames_mapping_ = nullptr;
    }
    copy_.Reset();

    char name[64];
    mello_hook::object_name(name, sizeof(name), MELLO_HOOK_NAME_FRAMES, pid_);
    frames_mapping_ = OpenFileMappingA(FILE_MAP_READ, FALSE, name);
    if (!frames_mapping_) {
        MELLO_LOG_ERROR(TAG, "the hook's frame block for pid=%u is missing: %lu", pid_,
                        GetLastError());
        failed_.store(true, std::memory_order_relaxed);
        return false;
    }
    const size_t total = static_cast<size_t>(bytes) * MELLO_HOOK_TEXTURE_COUNT;
    frames_ = static_cast<const uint8_t*>(
        MapViewOfFile(frames_mapping_, FILE_MAP_READ, 0, 0, total));
    if (!frames_) {
        MELLO_LOG_ERROR(TAG, "cannot map the hook's frame block: %lu", GetLastError());
        failed_.store(true, std::memory_order_relaxed);
        return false;
    }

    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = info_->width;
    desc.Height = info_->height;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = static_cast<DXGI_FORMAT>(info_->dxgi_format);
    desc.SampleDesc.Count = 1;
    // Written once a frame from the capture thread, read by the preprocessor.
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
    if (FAILED(device_->CreateTexture2D(&desc, nullptr, copy_.GetAddressOf()))) {
        MELLO_LOG_ERROR(TAG, "cannot create the frame texture %ux%u", desc.Width, desc.Height);
        failed_.store(true, std::memory_order_relaxed);
        return false;
    }

    width_ = desc.Width;
    height_ = desc.Height;
    frame_bytes_ = bytes;
    frame_pitch_ = pitch;
    texture_generation_ = as_atomic_u32(&info_->texture_generation)->load(std::memory_order_acquire);
    MELLO_LOG_INFO(TAG, "hook frames are %ux%u fmt=%u through memory (%u bytes a frame)",
                   width_.load(), height_.load(), info_->dxgi_format, bytes);
    return true;
}

bool HookCapture::upload_memory_frame(uint32_t slot) {
    if (!frames_ || !copy_) return false;
    const uint8_t* source = frames_ + static_cast<size_t>(slot) * frame_bytes_;
    // One upload, no intermediate copy: the source rows are already the pitch
    // the hook wrote them at.
    context_->UpdateSubresource(copy_.Get(), 0, nullptr, source, frame_pitch_, 0);
    return true;
}

// Opens the textures the hook published. Called when the generation changes:
// at the first frame, and after a resize.
bool HookCapture::refresh_textures() {
    if ((info_->flags & MELLO_HOOK_FLAG_CPU_COPY) != 0) {
        const uint32_t generation =
            as_atomic_u32(&info_->texture_generation)->load(std::memory_order_acquire);
        if (generation == texture_generation_ && copy_) return true;
        return refresh_memory_frames();
    }

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
    MELLO_LOG_INFO(TAG, "hook frames are %ux%u fmt=%u (generation %u)", width_.load(), height_.load(),
                   static_cast<unsigned>(desc.Format), generation);
    return true;
}

bool HookCapture::start(uint32_t target_fps, FrameCallback callback) {
    if (!info_) return false;
    if (running_.load()) return true;

    target_fps_ = target_fps == 0 ? 60 : target_fps;
    // The hook copies at this rate, not at the game's: a present between two
    // due frames costs the game no copy (and on the Direct3D 9 memory path,
    // no read-back stall).
    as_atomic_u32(&info_->target_fps)->store(target_fps_, std::memory_order_relaxed);
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
    cadence_deadline_us_ = 0;

    last_presents_change_us_.store(now_us(), std::memory_order_relaxed);

    while (running_.load()) {
        beat();

        // Sample the game's present count on every turn, frame or no frame.
        // This is what says whether the game is drawing at all, and the loop
        // runs at least every 100 ms whatever the game does.
        {
            const uint64_t presents = as_atomic_u64(&info_->presents_seen)
                                          ->load(std::memory_order_relaxed);
            if (presents != last_presents_.load(std::memory_order_relaxed)) {
                last_presents_.store(presents, std::memory_order_relaxed);
                last_presents_change_us_.store(now_us(), std::memory_order_relaxed);
            }
        }

        if (WaitForSingleObject(frame_event_, 100) != WAIT_OBJECT_0) {
            // No frame. A game that presents nothing is quiet, not broken: the
            // ladder rule for that lives in ProcessCapture.
            continue;
        }

        const uint64_t index = as_atomic_u64(&info_->frame_index)->load(std::memory_order_acquire);
        if (index == last_frame_index_) continue;
        last_frame_index_ = index;

        const uint32_t error = as_atomic_u32(&info_->last_error)->load(std::memory_order_relaxed);
        if (error_is_transient(error)) {
            // The hook recovers by itself. Keep waiting; the ladder gives this
            // state its own, longer deadline.
            continue;
        }
        if (error_is_fatal(error)) {
            MELLO_LOG_ERROR(TAG, "the hook stopped capturing in pid=%u: %s", pid_,
                            error_name(error));
            failed_.store(true, std::memory_order_relaxed);
            break;
        }

        if (!refresh_textures()) break;

        // Deliver at the target rate. The game presents at its own rate, which
        // can be several times the stream's. The decision comes before the
        // copy, so a present that is not delivered costs no copy. The rule is
        // the pipeline's: a reset to the arrival time gave 48 fps from a
        // 144 fps game for a 60 fps target.
        const uint64_t now = now_us();
        if (!frame_cadence_accepts(now, cadence_deadline_us_, target_fps_)) continue;

        const uint32_t slot = info_->texture_index % MELLO_HOOK_TEXTURE_COUNT;
        if (frames_) {
            if (!upload_memory_frame(slot)) continue;
        } else {
            if (!shared_[slot] || !copy_) continue;
            // The copy takes the frame away from the hook's pair, so the game
            // can keep presenting into them while the pipeline reads this one.
            context_->CopyResource(copy_.Get(), shared_[slot].Get());
        }

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

namespace {

// The shape of a Win32 cursor as straight-alpha BGRA, and its hotspot.
bool cursor_shape_of(HCURSOR cursor, CursorData& shape, int32_t* hot_x, int32_t* hot_y) {
    ICONINFO info{};
    if (!GetIconInfo(cursor, &info)) return false;
    struct Bitmaps {
        ICONINFO& i;
        ~Bitmaps() {
            if (i.hbmColor) DeleteObject(i.hbmColor);
            if (i.hbmMask) DeleteObject(i.hbmMask);
        }
    } owned{info};

    HDC dc = GetDC(nullptr);
    if (!dc) return false;
    bool ok = false;
    std::vector<uint8_t> bgra;
    BITMAP bm{};
    uint32_t width = 0, height = 0;
    if (info.hbmColor && GetObject(info.hbmColor, sizeof(bm), &bm) && bm.bmWidth > 0 &&
        bm.bmHeight > 0) {
        width = static_cast<uint32_t>(bm.bmWidth);
        height = static_cast<uint32_t>(bm.bmHeight);
        BITMAPINFO bi{};
        bi.bmiHeader.biSize = sizeof(bi.bmiHeader);
        bi.bmiHeader.biWidth = bm.bmWidth;
        bi.bmiHeader.biHeight = -bm.bmHeight;  // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        std::vector<uint8_t> pixels(static_cast<size_t>(width) * height * 4);
        if (GetDIBits(dc, info.hbmColor, 0, height, pixels.data(), &bi, DIB_RGB_COLORS) ==
            static_cast<int>(height)) {
            bool has_alpha = false;
            for (size_t i = 3; i < pixels.size(); i += 4) {
                if (pixels[i]) { has_alpha = true; break; }
            }
            ok = pointer_shape_to_bgra(PointerShapeKind::Color, width, height, width * 4,
                                       pixels.data(), pixels.size(), bgra);
            if (ok && !has_alpha && info.hbmMask) {
                // An old colour cursor: no alpha, the AND mask says which
                // pixels are the cursor.
                const uint32_t pitch = ((width + 31) / 32) * 4;
                std::vector<uint8_t> mask(static_cast<size_t>(pitch) * height);
                struct { BITMAPINFOHEADER h; RGBQUAD c[2]; } mi{};
                mi.h.biSize = sizeof(mi.h);
                mi.h.biWidth = static_cast<LONG>(width);
                mi.h.biHeight = -static_cast<LONG>(height);
                mi.h.biPlanes = 1;
                mi.h.biBitCount = 1;
                mi.h.biCompression = BI_RGB;
                if (GetDIBits(dc, info.hbmMask, 0, height, mask.data(),
                              reinterpret_cast<BITMAPINFO*>(&mi), DIB_RGB_COLORS) ==
                    static_cast<int>(height)) {
                    for (uint32_t y = 0; y < height; ++y) {
                        for (uint32_t x = 0; x < width; ++x) {
                            const bool transparent =
                                (mask[static_cast<size_t>(y) * pitch + x / 8] & (0x80 >> (x % 8))) != 0;
                            bgra[(static_cast<size_t>(y) * width + x) * 4 + 3] = transparent ? 0 : 255;
                        }
                    }
                }
            }
        }
    } else if (info.hbmMask && GetObject(info.hbmMask, sizeof(bm), &bm) && bm.bmWidth > 0 &&
               bm.bmHeight > 1) {
        // A monochrome cursor: one bitmap, the AND mask over the XOR mask.
        width = static_cast<uint32_t>(bm.bmWidth);
        height = static_cast<uint32_t>(bm.bmHeight) / 2;
        const uint32_t pitch = ((width + 31) / 32) * 4;
        std::vector<uint8_t> mask(static_cast<size_t>(pitch) * height * 2);
        struct { BITMAPINFOHEADER h; RGBQUAD c[2]; } mi{};
        mi.h.biSize = sizeof(mi.h);
        mi.h.biWidth = static_cast<LONG>(width);
        mi.h.biHeight = -static_cast<LONG>(height * 2);
        mi.h.biPlanes = 1;
        mi.h.biBitCount = 1;
        mi.h.biCompression = BI_RGB;
        if (GetDIBits(dc, info.hbmMask, 0, height * 2, mask.data(),
                      reinterpret_cast<BITMAPINFO*>(&mi), DIB_RGB_COLORS) ==
            static_cast<int>(height * 2)) {
            ok = pointer_shape_to_bgra(PointerShapeKind::Monochrome, width, height, pitch,
                                       mask.data(), mask.size(), bgra);
        }
    }
    ReleaseDC(nullptr, dc);
    if (!ok || width > 0xFFFF || height > 0xFFFF) return false;

    shape.shape_w = static_cast<uint16_t>(width);
    shape.shape_h = static_cast<uint16_t>(height);
    shape.shape_bgra = std::move(bgra);
    ++shape.shape_serial;
    *hot_x = static_cast<int32_t>(info.xHotspot);
    *hot_y = static_cast<int32_t>(info.yHotspot);
    return true;
}

} // namespace

bool HookCapture::get_cursor(CursorData& out) {
    CURSORINFO ci{};
    ci.cbSize = sizeof(ci);
    if (!GetCursorInfo(&ci)) return false;

    std::lock_guard<std::mutex> lock(cursor_mutex_);
    out.visible = false;

    // The game's window, looked up again once a second: a game can replace
    // its window when it changes display mode.
    const uint64_t now = now_us();
    if (!cursor_window_ || !IsWindow(cursor_window_) ||
        now - cursor_window_checked_us_ > 1'000'000) {
        cursor_window_ = find_main_window(pid_);
        cursor_window_checked_us_ = now;
    }
    if (!cursor_window_ || (ci.flags & CURSOR_SHOWING) == 0 || !ci.hCursor) return true;

    // Only a pointer over the game is in the picture. A pointer over another
    // window on top of the game is not, and neither is one beside it.
    HWND under = WindowFromPoint(ci.ptScreenPos);
    DWORD under_pid = 0;
    if (under) GetWindowThreadProcessId(GetAncestor(under, GA_ROOT), &under_pid);
    if (under_pid != pid_) return true;

    RECT client{};
    POINT p = ci.ptScreenPos;
    if (!GetClientRect(cursor_window_, &client) || !ScreenToClient(cursor_window_, &p)) return true;
    const LONG client_w = client.right - client.left;
    const LONG client_h = client.bottom - client.top;
    if (client_w <= 0 || client_h <= 0 || p.x < 0 || p.y < 0 || p.x >= client_w ||
        p.y >= client_h) {
        return true;
    }

    if (ci.hCursor != cursor_handle_) {
        cursor_handle_ = ci.hCursor;
        if (!cursor_shape_of(ci.hCursor, cursor_shape_, &cursor_hotspot_x_, &cursor_hotspot_y_)) {
            cursor_shape_.shape_w = cursor_shape_.shape_h = 0;
            cursor_shape_.shape_bgra.clear();
            ++cursor_shape_.shape_serial;
        }
    }
    if (cursor_shape_.shape_bgra.empty()) return true;

    // The back buffer can have another size than the window (a game that
    // renders at 1280x720 in a 2560x1440 window), and the hook's frames have
    // the back buffer's size.
    const double sx = static_cast<double>(width_.load()) / client_w;
    const double sy = static_cast<double>(height_.load()) / client_h;
    out.x = static_cast<int32_t>(p.x * sx) - cursor_hotspot_x_;
    out.y = static_cast<int32_t>(p.y * sy) - cursor_hotspot_y_;
    out.visible = true;
    out.shape_changed = false;
    if (out.shape_serial != cursor_shape_.shape_serial) {
        out.shape_serial = cursor_shape_.shape_serial;
        out.shape_w = cursor_shape_.shape_w;
        out.shape_h = cursor_shape_.shape_h;
        out.shape_bgra = cursor_shape_.shape_bgra;
        out.shape_changed = true;
    }
    return true;
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
