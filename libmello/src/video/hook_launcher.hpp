#pragma once
#ifdef _WIN32

#include <cstdint>
#include <string>

#include "mello_hook_protocol.h"

namespace mello::video::hook {

/// Present-function offsets for this machine, from the offsets helper.
///
/// The offsets come from a helper process, never from a probe device inside the
/// game (plan 3.3). They are cached for the life of the client, keyed by the
/// file version of dxgi.dll: a Windows update changes the file and the offsets
/// with it.
struct Offsets {
    bool        valid = false;
    std::string dxgi_file_version;
    uint64_t    dxgi_present = 0;
    uint64_t    dxgi_present1 = 0;
    uint64_t    dxgi_resize_buffers = 0;
};

/// Runs the offsets helper for `bits` (32 or 64) and returns what it printed.
/// The second call for the same bitness returns the cached answer.
const Offsets& offsets_for(int bits);

/// Result of an injection attempt, for the log and for telemetry.
enum class InjectResult {
    Ready,          // the hook signalled ready
    HelperMissing,  // the helper binary is not in the install
    HelperFailed,   // the helper ran and refused
    NoWindowThread, // the game has no window thread to hook
    Timeout,        // the hook did not load in time
};

const char* inject_result_name(InjectResult result);

/// Loads the hook into `pid` with the injection helper of the matching
/// bitness. The caller must have created the shared block and the events first:
/// the helper waits on the ready event, and the hook needs the block to find
/// its offsets.
InjectResult inject(uint32_t pid, int bits, uint32_t timeout_ms);

/// Directory that holds the hook binaries. It is `MELLO_HOOK_DIR` when that is
/// set, and the folder of the running executable otherwise. Empty when neither
/// holds the binaries.
std::string hook_directory();

} // namespace mello::video::hook

#endif // _WIN32
