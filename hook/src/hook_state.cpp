#include "hook_state.hpp"

#include <atomic>

#include "hook_log.hpp"

namespace mello_hook {
namespace {

// Reads and writes on the shared block use these, not plain assignments, so the
// compiler cannot move the frame counter before the data it describes.
inline std::atomic<uint64_t>* as_atomic_u64(uint64_t* p) {
    return reinterpret_cast<std::atomic<uint64_t>*>(p);
}
inline std::atomic<uint32_t>* as_atomic_u32(uint32_t* p) {
    return reinterpret_cast<std::atomic<uint32_t>*>(p);
}

static_assert(sizeof(std::atomic<uint64_t>) == sizeof(uint64_t), "atomic u64 must not add padding");
static_assert(sizeof(std::atomic<uint32_t>) == sizeof(uint32_t), "atomic u32 must not add padding");

}  // namespace

int64_t qpc_now() {
    LARGE_INTEGER v{};
    QueryPerformanceCounter(&v);
    return v.QuadPart;
}

HookState& HookState::instance() {
    static HookState state;
    return state;
}

bool HookState::open() {
    if (info_) return true;

    const uint32_t pid = GetCurrentProcessId();
    char name[64];

    object_name(name, sizeof(name), MELLO_HOOK_NAME_INFO, pid);
    mapping_ = OpenFileMappingA(FILE_MAP_ALL_ACCESS, FALSE, name);
    if (!mapping_) {
        // No block: m3llo is not asking this process for frames.
        return false;
    }

    void* view = MapViewOfFile(mapping_, FILE_MAP_ALL_ACCESS, 0, 0, sizeof(MelloHookInfo));
    if (!view) {
        log_line("shared block found but the view failed: %lu", GetLastError());
        close();
        return false;
    }
    auto* info = static_cast<MelloHookInfo*>(view);

    // The hook and the client ship in different files. A mismatch here means an
    // update left one of them behind, and the layout below cannot be trusted.
    if (info->protocol_version != MELLO_HOOK_PROTOCOL_VERSION ||
        info->struct_size != sizeof(MelloHookInfo)) {
        log_line("protocol mismatch: client has v%u size %u, hook has v%u size %u",
                 info->protocol_version, info->struct_size,
                 MELLO_HOOK_PROTOCOL_VERSION, static_cast<unsigned>(sizeof(MelloHookInfo)));
        UnmapViewOfFile(view);
        close();
        return false;
    }

    object_name(name, sizeof(name), MELLO_HOOK_NAME_READY, pid);
    ready_event_ = OpenEventA(EVENT_MODIFY_STATE, FALSE, name);
    object_name(name, sizeof(name), MELLO_HOOK_NAME_FRAME, pid);
    frame_event_ = OpenEventA(EVENT_MODIFY_STATE, FALSE, name);
    object_name(name, sizeof(name), MELLO_HOOK_NAME_STOP, pid);
    stop_event_ = OpenEventA(SYNCHRONIZE, FALSE, name);

    if (!ready_event_ || !frame_event_ || !stop_event_) {
        log_line("shared block found but an event is missing: %lu", GetLastError());
        UnmapViewOfFile(view);
        close();
        return false;
    }

    LARGE_INTEGER freq{};
    QueryPerformanceFrequency(&freq);
    qpc_frequency_ = freq.QuadPart;

    info_ = info;
    info_->hook_pid = pid;
    info_->hook_bitness = MELLO_HOOK_BITS;
    return true;
}

void HookState::close() {
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

bool HookState::client_alive() const {
    if (!info_ || qpc_frequency_ <= 0) return false;
    const int64_t beat = static_cast<int64_t>(
        as_atomic_u64(const_cast<uint64_t*>(&info_->heartbeat_qpc))->load(std::memory_order_relaxed));
    if (beat == 0) return false;
    const int64_t age_ms = (qpc_now() - beat) * 1000 / qpc_frequency_;
    return age_ms >= 0 && age_ms < static_cast<int64_t>(MELLO_HOOK_HEARTBEAT_TIMEOUT_MS);
}

bool HookState::capture_wanted() const {
    if (!info_) return false;
    if (as_atomic_u32(const_cast<uint32_t*>(&info_->capture_enabled))
            ->load(std::memory_order_relaxed) == 0) {
        return false;
    }
    return client_alive();
}

void HookState::publish_description(uint32_t api, uint32_t width, uint32_t height,
                                    uint32_t dxgi_format, uint64_t adapter_luid,
                                    const uint32_t* handles, uint32_t flags) {
    if (!info_) return;
    info_->api = api;
    info_->width = width;
    info_->height = height;
    info_->dxgi_format = dxgi_format;
    info_->adapter_luid = adapter_luid;
    for (uint32_t i = 0; i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
        info_->shared_handles[i] = handles[i];
    }
    info_->flags = flags;
    // The generation changes last: the client re-opens the textures when it
    // sees a new value, so every field above must already be in place.
    as_atomic_u32(&info_->texture_generation)
        ->fetch_add(1, std::memory_order_release);
}

void HookState::publish_frame(uint32_t texture_index, uint64_t qpc) {
    if (!info_) return;
    info_->texture_index = texture_index;
    info_->frame_qpc = qpc;
    as_atomic_u64(&info_->frame_index)->fetch_add(1, std::memory_order_release);
}

void HookState::set_error(uint32_t error) {
    if (!info_) return;
    as_atomic_u32(&info_->last_error)->store(error, std::memory_order_relaxed);
}

void HookState::count_drop() {
    if (!info_) return;
    as_atomic_u64(&info_->frames_dropped)->fetch_add(1, std::memory_order_relaxed);
}

void HookState::count_present() {
    if (!info_) return;
    as_atomic_u64(&info_->presents_seen)->fetch_add(1, std::memory_order_relaxed);
}

void HookState::count_fault() {
    if (!info_) return;
    as_atomic_u64(&info_->faults)->fetch_add(1, std::memory_order_relaxed);
}

void HookState::signal_ready() {
    if (ready_event_) SetEvent(ready_event_);
}

void HookState::signal_frame() {
    if (frame_event_) SetEvent(frame_event_);
}

bool HookState::wait_for_stop(DWORD wait_ms) const {
    if (!stop_event_) return true;
    return WaitForSingleObject(stop_event_, wait_ms) == WAIT_OBJECT_0;
}

}  // namespace mello_hook
