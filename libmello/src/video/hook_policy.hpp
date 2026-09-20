#pragma once
#ifdef _WIN32

#include <cstdint>
#include <string>

namespace mello::video::hook {

/// Why the hook may or may not go into one process.
///
/// These are the run-time checks from plan 3.6, and they run on every stream
/// start, not once. The catalogue and the backend decide whether a game is
/// allowed at all; these checks decide whether this process, right now, is the
/// one the catalogue meant.
///
/// Bob's rule: the hook must not get anyone banned. Any doubt means no hook,
/// and the capture ladder falls back to screen capture.
enum class PolicyVerdict {
    Allowed,
    NotAllowedByCaller,   // the catalogue or the backend said no
    ProcessUnreadable,    // OpenProcess was refused: a protected process
    AntiCheatModule,      // an anti-cheat DLL is loaded in the game
    AntiCheatProcess,     // an anti-cheat service or launcher is running
    StorePackaged,        // installed under WindowsApps
    Chromium,             // a Chromium shell, which has no present to hook
    Elevated,             // runs with more rights than m3llo has
};

const char* verdict_name(PolicyVerdict verdict);

struct PolicyResult {
    PolicyVerdict verdict = PolicyVerdict::NotAllowedByCaller;
    /// The anti-cheat module or process that decided it, when there was one.
    std::string   detail;

    bool allowed() const { return verdict == PolicyVerdict::Allowed; }
};

/// Runs every run-time check against one process. `allowed_by_caller` carries
/// the catalogue and backend decision, which the client makes before this.
PolicyResult check_process(uint32_t pid, bool allowed_by_caller);

// --- The parts that are worth testing without a game ---

/// True when this module name belongs to an anti-cheat product. Matching is by
/// name, case-insensitive, because the products ship under many paths.
bool is_anticheat_module(const std::string& module_name);

/// True when this process name is an anti-cheat service or launcher.
bool is_anticheat_process(const std::string& process_name);

/// True for a Store-packaged install. OBS cannot hook these either.
bool is_store_packaged(const std::string& executable_path);

/// True for a Chromium shell window class. Those games render in a process
/// that does not present through the APIs the hook detours.
bool is_chromium_window_class(const std::string& window_class);

/// Name of the environment variable that allows the hook for one executable.
///
/// It stands in for the backend `hook_allow` list until the `capture` block in
/// the `start_stream` response exists (plan 3.6). It is for testing the hook
/// against a real game, it names exactly one executable, and it changes nothing
/// else: every run-time check below still has to pass.
constexpr const char* kDeveloperAllowVariable = "MELLO_HOOK_ALLOW_EXE";

/// True when `kDeveloperAllowVariable` names this executable. `variable_value`
/// is the raw value, so this can be tested without touching the environment.
bool developer_allows(const std::string& variable_value, const std::string& executable_path);

} // namespace mello::video::hook

#endif // _WIN32
