#include "hook_d3d12.hpp"

#include <windows.h>

#include <d3d12.h>
#include <dxgi1_4.h>

#include <atomic>

#include <detours/detours.h>

#include "hook_log.hpp"
#include "hook_state.hpp"

namespace mello_hook {
namespace {

using ExecuteFn = void(STDMETHODCALLTYPE*)(ID3D12CommandQueue*, UINT, ID3D12CommandList* const*);

// ID3D12CommandQueue::ExecuteCommandLists in the vtable: IUnknown (3),
// ID3D12Object (4), ID3D12DeviceChild (1), then UpdateTileMappings,
// CopyTileMappings, ExecuteCommandLists. The COM ABI fixes it.
constexpr int kVtExecuteCommandLists = 10;

ExecuteFn         g_real_execute = nullptr;
std::atomic<bool> g_execute_hooked{false};
// Set when the ExecuteCommandLists detour cannot be installed, or faulted.
// The D3D12 path is then off for good; the D3D11 path is not affected.
std::atomic<bool> g_d3d12_off{false};

// The direct queues the game submits to. Each one is AddRef'd once and kept
// for the life of the process: the present path reads these pointers with no
// lock, so a queue must never be released under it. A game makes a handful of
// queues at start; the table is bounded and a ninth queue is not tracked.
constexpr int kMaxQueues = 8;
std::atomic<ID3D12CommandQueue*> g_queues[kMaxQueues];
std::atomic<bool>                g_queue_direct[kMaxQueues];
std::atomic<int>                 g_queue_slots{0};
// The direct queue seen inside a D3D12 Present on the presenting thread: the
// swap chain's own queue, when DXGI submits on it while it presents.
std::atomic<ID3D12CommandQueue*> g_present_queue{nullptr};
// The last direct queue the game submitted to, outside a Present.
std::atomic<ID3D12CommandQueue*> g_last_direct{nullptr};
// The thread inside a D3D12 Present, 0 when none.
std::atomic<DWORD>               g_presenting_thread{0};

// The table slot of `queue`, or -1.
int queue_slot(ID3D12CommandQueue* queue) {
    for (int i = 0; i < kMaxQueues; ++i) {
        if (g_queues[i].load(std::memory_order_acquire) == queue) return i;
    }
    return -1;
}

// Body of the ExecuteCommandLists detour: a known queue costs a few loads; a
// new one costs a GetDesc and an AddRef, once.
void note_queue(ID3D12CommandQueue* queue) {
    if (!queue) return;
    int slot = queue_slot(queue);
    if (slot < 0) {
        slot = g_queue_slots.fetch_add(1, std::memory_order_relaxed);
        if (slot >= kMaxQueues) return;
        // Copy and compute queues stay in the table too, so their next call
        // takes the fast path; they are never used for a copy.
        g_queue_direct[slot].store(queue->GetDesc().Type == D3D12_COMMAND_LIST_TYPE_DIRECT,
                                   std::memory_order_relaxed);
        queue->AddRef();
        g_queues[slot].store(queue, std::memory_order_release);
    }
    if (!g_queue_direct[slot].load(std::memory_order_relaxed)) return;
    if (g_presenting_thread.load(std::memory_order_relaxed) == GetCurrentThreadId()) {
        g_present_queue.store(queue, std::memory_order_release);
    } else {
        g_last_direct.store(queue, std::memory_order_release);
    }
}

void STDMETHODCALLTYPE hooked_execute(ID3D12CommandQueue* queue, UINT count,
                                      ID3D12CommandList* const* lists) {
    if (!g_d3d12_off.load(std::memory_order_relaxed)) {
        __try {
            note_queue(queue);
        } __except (EXCEPTION_EXECUTE_HANDLER) {
            g_d3d12_off.store(true);
            HookState::instance().count_fault();
        }
    }
    g_real_execute(queue, count, lists);
}

// --- Capture resources. Present thread only. Raw pointers: the functions
// that touch them run under a structured exception guard. ---

struct Resources {
    IUnknown*                  swap = nullptr;    // identity only; never called
    ID3D12Device*              device = nullptr;
    ID3D12CommandQueue*        queue = nullptr;   // one of g_queues; not owned
    ID3D12Resource*            texture[MELLO_HOOK_TEXTURE_COUNT]{};
    HANDLE                     texture_handle[MELLO_HOOK_TEXTURE_COUNT]{};
    ID3D12Fence*               fence = nullptr;
    HANDLE                     fence_handle = nullptr;
    ID3D12CommandAllocator*    allocator[MELLO_HOOK_TEXTURE_COUNT]{};
    ID3D12GraphicsCommandList* list[MELLO_HOOK_TEXTURE_COUNT]{};
    uint64_t                   slot_value[MELLO_HOOK_TEXTURE_COUNT]{};
    uint64_t                   fence_value = 0;
    UINT                       width = 0;
    UINT                       height = 0;
    DXGI_FORMAT                format = DXGI_FORMAT_UNKNOWN;
    bool                       ready = false;
};

Resources g_res;

void release_resources() {
    for (uint32_t i = 0; i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
        if (g_res.list[i]) g_res.list[i]->Release();
        if (g_res.allocator[i]) g_res.allocator[i]->Release();
        if (g_res.texture[i]) g_res.texture[i]->Release();
        if (g_res.texture_handle[i]) CloseHandle(g_res.texture_handle[i]);
    }
    if (g_res.fence) g_res.fence->Release();
    if (g_res.fence_handle) CloseHandle(g_res.fence_handle);
    if (g_res.device) g_res.device->Release();
    g_res = Resources{};
}

bool format_is_hdr(DXGI_FORMAT f) {
    return f == DXGI_FORMAT_R16G16B16A16_FLOAT || f == DXGI_FORMAT_R10G10B10A2_UNORM;
}

// Installs the ExecuteCommandLists detour. The address comes from a command
// queue made on the game's own device and released at once: the function the
// game's queues run is the one in this process, which may be a D3D12Core.dll
// the game ships (the Agility SDK), so an offset of the system DLL could point
// anywhere. Once per process, on the first D3D12 present.
bool install_execute_hook(ID3D12Device* device) {
    if (g_execute_hooked.load()) return true;
    if (g_d3d12_off.load()) return false;

    D3D12_COMMAND_QUEUE_DESC desc{};
    desc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    ID3D12CommandQueue* probe = nullptr;
    if (FAILED(device->CreateCommandQueue(&desc, __uuidof(ID3D12CommandQueue),
                                          reinterpret_cast<void**>(&probe))) ||
        !probe) {
        log_line("D3D12: no command queue on the game's device; D3D12 capture is off");
        g_d3d12_off.store(true);
        return false;
    }
    void** vtable = *reinterpret_cast<void***>(probe);
    g_real_execute = reinterpret_cast<ExecuteFn>(vtable[kVtExecuteCommandLists]);
    probe->Release();

    DetourTransactionBegin();
    DetourUpdateThread(GetCurrentThread());
    DetourAttach(reinterpret_cast<PVOID*>(&g_real_execute), reinterpret_cast<PVOID>(hooked_execute));
    const LONG result = DetourTransactionCommit();
    if (result != NO_ERROR) {
        log_line("D3D12: Detours refused ExecuteCommandLists: %ld; D3D12 capture is off", result);
        g_d3d12_off.store(true);
        return false;
    }
    g_execute_hooked.store(true);
    log_line("D3D12: hooked ExecuteCommandLists");
    return true;
}

// The queue to copy on: the one seen inside Present when DXGI submits there,
// else the last direct queue the game used. Either must belong to this device.
ID3D12CommandQueue* pick_queue(ID3D12Device* device) {
    ID3D12CommandQueue* candidates[2] = {g_present_queue.load(std::memory_order_acquire),
                                         g_last_direct.load(std::memory_order_acquire)};
    for (ID3D12CommandQueue* queue : candidates) {
        if (!queue) continue;
        if (queue == g_res.queue) return queue;  // checked when it was taken
        ID3D12Device* owner = nullptr;
        if (SUCCEEDED(queue->GetDevice(__uuidof(ID3D12Device), reinterpret_cast<void**>(&owner))) &&
            owner) {
            const bool same = owner == device;
            owner->Release();
            if (same) return queue;
        }
    }
    return nullptr;
}

bool build_resources(IUnknown* identity, ID3D12Device* device, const DXGI_SWAP_CHAIN_DESC& desc) {
    release_resources();
    HookState& state = HookState::instance();

    D3D12_HEAP_PROPERTIES heap{};
    heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    D3D12_RESOURCE_DESC td{};
    td.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    td.Width = desc.BufferDesc.Width;
    td.Height = desc.BufferDesc.Height;
    td.DepthOrArraySize = 1;
    td.MipLevels = 1;
    td.Format = desc.BufferDesc.Format;
    td.SampleDesc.Count = 1;
    td.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
    td.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;

    uint32_t handles[MELLO_HOOK_TEXTURE_COUNT]{};
    for (uint32_t i = 0; i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
        if (FAILED(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_SHARED, &td,
                                                   D3D12_RESOURCE_STATE_COMMON, nullptr,
                                                   __uuidof(ID3D12Resource),
                                                   reinterpret_cast<void**>(&g_res.texture[i]))) ||
            FAILED(device->CreateSharedHandle(g_res.texture[i], nullptr, GENERIC_ALL, nullptr,
                                              &g_res.texture_handle[i])) ||
            FAILED(device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT,
                                                  __uuidof(ID3D12CommandAllocator),
                                                  reinterpret_cast<void**>(&g_res.allocator[i]))) ||
            FAILED(device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, g_res.allocator[i],
                                             nullptr, __uuidof(ID3D12GraphicsCommandList),
                                             reinterpret_cast<void**>(&g_res.list[i])))) {
            log_line("D3D12: shared texture %u of %ux%u fmt=%u failed", i, desc.BufferDesc.Width,
                     desc.BufferDesc.Height, static_cast<unsigned>(desc.BufferDesc.Format));
            release_resources();
            state.set_error(MELLO_HOOK_ERR_SHARED_TEXTURE);
            return false;
        }
        g_res.list[i]->Close();
        // An NT handle value fits in 32 bits, from a 32-bit or a 64-bit game.
        handles[i] = static_cast<uint32_t>(reinterpret_cast<uintptr_t>(g_res.texture_handle[i]));
    }
    if (FAILED(device->CreateFence(0, D3D12_FENCE_FLAG_SHARED, __uuidof(ID3D12Fence),
                                   reinterpret_cast<void**>(&g_res.fence))) ||
        FAILED(device->CreateSharedHandle(g_res.fence, nullptr, GENERIC_ALL, nullptr,
                                          &g_res.fence_handle))) {
        log_line("D3D12: shared fence failed");
        release_resources();
        state.set_error(MELLO_HOOK_ERR_SHARED_TEXTURE);
        return false;
    }

    device->AddRef();
    g_res.device = device;
    g_res.swap = identity;
    g_res.width = desc.BufferDesc.Width;
    g_res.height = desc.BufferDesc.Height;
    g_res.format = desc.BufferDesc.Format;
    g_res.ready = true;

    const LUID luid = device->GetAdapterLuid();
    const uint64_t adapter = (static_cast<uint64_t>(static_cast<uint32_t>(luid.HighPart)) << 32) |
                             static_cast<uint32_t>(luid.LowPart);
    uint32_t flags = MELLO_HOOK_FLAG_NT_HANDLES;
    if (format_is_hdr(g_res.format)) flags |= MELLO_HOOK_FLAG_HDR;
    if (MelloHookInfo* info = state.info()) {
        info->fence_handle = static_cast<uint32_t>(reinterpret_cast<uintptr_t>(g_res.fence_handle));
        info->frame_fence_value = 0;
    }
    // The generation moves last, so the client sees the fence handle with
    // the textures.
    state.publish_description(MELLO_HOOK_API_D3D12, g_res.width, g_res.height,
                              static_cast<uint32_t>(g_res.format), adapter, handles, flags);
    state.set_error(MELLO_HOOK_OK);
    log_line("D3D12: capturing %ux%u fmt=%u, handles=%08x,%08x fence=%08x", g_res.width,
             g_res.height, static_cast<unsigned>(g_res.format), handles[0], handles[1],
             static_cast<unsigned>(reinterpret_cast<uintptr_t>(g_res.fence_handle)));
    return true;
}

void transition(ID3D12Resource* resource, D3D12_RESOURCE_STATES before,
                D3D12_RESOURCE_STATES after, D3D12_RESOURCE_BARRIER* out) {
    out->Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    out->Flags = D3D12_RESOURCE_BARRIER_FLAG_NONE;
    out->Transition.pResource = resource;
    out->Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    out->Transition.StateBefore = before;
    out->Transition.StateAfter = after;
}

}  // namespace

uint64_t d3d12_target_area() {
    return g_res.ready ? static_cast<uint64_t>(g_res.width) * g_res.height : 0;
}

D3d12Present d3d12_capture_present(IDXGISwapChain* swap, IUnknown* identity,
                                   const DXGI_SWAP_CHAIN_DESC& desc, uint64_t other_area,
                                   HookState& state) {
    if (g_d3d12_off.load(std::memory_order_relaxed)) {
        return identity == g_res.swap && g_res.ready ? D3d12Present::Handled
                                                     : D3d12Present::NotD3d12;
    }

    ID3D12Device* device = nullptr;
    if (identity == g_res.swap && g_res.ready) {
        device = g_res.device;
    } else {
        // A swap chain this path has not taken. Only a D3D12 one answers an
        // ID3D12Device. The reference is dropped at once: the device outlives
        // its swap chain, and build_resources takes its own.
        ID3D12Device* probe = nullptr;
        if (FAILED(swap->GetDevice(__uuidof(ID3D12Device), reinterpret_cast<void**>(&probe))) ||
            !probe) {
            return D3d12Present::NotD3d12;
        }
        probe->Release();
        // A game can present several swap chains: keep the largest.
        const uint64_t area = static_cast<uint64_t>(desc.BufferDesc.Width) * desc.BufferDesc.Height;
        if (area <= other_area || (g_res.ready && area <= d3d12_target_area())) {
            return D3d12Present::Handled;
        }
        device = probe;
        if (!install_execute_hook(device)) {
            state.set_error(MELLO_HOOK_ERR_NO_DEVICE);
            return D3d12Present::Handled;
        }
    }

    const bool changed = !g_res.ready || g_res.swap != identity ||
                         g_res.width != desc.BufferDesc.Width ||
                         g_res.height != desc.BufferDesc.Height ||
                         g_res.format != desc.BufferDesc.Format;
    if (changed) {
        // A rebuild while a copy is still on the GPU would free what it uses.
        // Wait for it at a later present instead of here.
        if (g_res.ready && g_res.fence->GetCompletedValue() < g_res.fence_value) {
            return D3d12Present::Handled;
        }
        if (!build_resources(identity, device, desc)) return D3d12Present::Handled;
    }

    // The queue the game presents on. Until the game has submitted on one,
    // there is nothing to copy with: the next present tries again.
    ID3D12CommandQueue* queue = pick_queue(g_res.device);
    if (!queue) return D3d12Present::Handled;
    if (queue != g_res.queue) {
        // Once per queue change, like a resource build: not the steady path.
        log_line("D3D12: copies go on queue %p (%s)", static_cast<void*>(queue),
                 queue == g_present_queue.load() ? "submitted to inside Present"
                                                 : "the game's last direct queue");
        g_res.queue = queue;
    }

    if (!state.copy_due(qpc_now())) return D3d12Present::Handled;

    const uint32_t slot = static_cast<uint32_t>(g_res.fence_value % MELLO_HOOK_TEXTURE_COUNT);
    if (g_res.fence->GetCompletedValue() < g_res.slot_value[slot]) {
        // The last copy into this slot has not finished: the GPU is behind.
        // Never wait on the game's thread; skip this frame.
        state.count_drop();
        return D3d12Present::Handled;
    }

    IDXGISwapChain3* swap3 = nullptr;
    if (FAILED(swap->QueryInterface(__uuidof(IDXGISwapChain3), reinterpret_cast<void**>(&swap3))) ||
        !swap3) {
        state.count_drop();
        return D3d12Present::Handled;
    }
    ID3D12Resource* back = nullptr;
    const HRESULT got = swap3->GetBuffer(swap3->GetCurrentBackBufferIndex(),
                                         __uuidof(ID3D12Resource), reinterpret_cast<void**>(&back));
    swap3->Release();
    if (FAILED(got) || !back) {
        state.count_drop();
        return D3d12Present::Handled;
    }

    ID3D12CommandAllocator* allocator = g_res.allocator[slot];
    ID3D12GraphicsCommandList* list = g_res.list[slot];
    if (FAILED(allocator->Reset()) || FAILED(list->Reset(allocator, nullptr))) {
        back->Release();
        state.set_error(MELLO_HOOK_ERR_COPY);
        return D3d12Present::Handled;
    }
    // The back buffer is in the present state at Present, as D3D12 requires.
    D3D12_RESOURCE_BARRIER barriers[2]{};
    transition(back, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_COPY_SOURCE, &barriers[0]);
    transition(g_res.texture[slot], D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST,
               &barriers[1]);
    list->ResourceBarrier(2, barriers);
    list->CopyResource(g_res.texture[slot], back);
    transition(back, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_PRESENT, &barriers[0]);
    transition(g_res.texture[slot], D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON,
               &barriers[1]);
    list->ResourceBarrier(2, barriers);
    const HRESULT closed = list->Close();
    back->Release();
    if (FAILED(closed)) {
        state.set_error(MELLO_HOOK_ERR_COPY);
        return D3d12Present::Handled;
    }

    // On the game's queue: after the game's work for this frame, before the
    // flip that Present queues behind it.
    ID3D12CommandList* lists[] = {list};
    queue->ExecuteCommandLists(1, lists);
    const uint64_t value = g_res.fence_value + 1;
    if (FAILED(queue->Signal(g_res.fence, value))) {
        state.set_error(MELLO_HOOK_ERR_COPY);
        return D3d12Present::Handled;
    }
    g_res.fence_value = value;
    g_res.slot_value[slot] = value;

    // The client reads the slot from the fence value ((value - 1) % 2) and
    // waits for it on the GPU before it reads the texture. The value goes out
    // before the frame counter, which publish_frame writes last.
    if (MelloHookInfo* info = state.info()) {
        reinterpret_cast<std::atomic<uint64_t>*>(&info->frame_fence_value)
            ->store(value, std::memory_order_release);
    }
    state.count_copy();
    state.publish_frame(slot, static_cast<uint64_t>(qpc_now()));
    state.signal_frame();
    return D3d12Present::Handled;
}

void d3d12_mark_presenting(bool presenting) {
    g_presenting_thread.store(presenting ? GetCurrentThreadId() : 0, std::memory_order_relaxed);
}

bool d3d12_is_target(IUnknown* identity) { return g_res.ready && identity == g_res.swap; }

bool d3d12_release_when_idle() {
    if (!g_res.ready) return true;
    if (g_res.fence && g_res.fence->GetCompletedValue() < g_res.fence_value) return false;
    release_resources();
    return true;
}

void d3d12_abandon() {
    g_d3d12_off.store(true);
    g_res = Resources{};
}

}  // namespace mello_hook
