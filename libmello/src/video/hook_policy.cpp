#include "hook_policy.hpp"

#ifdef _WIN32

#include <windows.h>

#include <psapi.h>
#include <tlhelp32.h>

#include <algorithm>
#include <array>

#include "../util/log.hpp"

namespace mello::video::hook {
namespace {

constexpr const char* TAG = "video/hook";

// Anti-cheat modules, by the name the product loads into the game. The list is
// from the research doc and from the catalogue work in
// `mello-backlog/plans/game-capture-hook-games/`. A name that is not here is
// not proof of safety, which is why the catalogue gate comes first.
constexpr std::array<const char*, 14> kAntiCheatModules = {
    "easyanticheat",     // Epic EasyAntiCheat, both the old and the EOS build
    "eac",               //
    "beclient",          // BattlEye
    "beservice",         //
    "ace-",              // Anti Cheat Expert (Tencent)
    "anticheatexpert",   //
    "gameguard",         // nProtect GameGuard
    "npggnt",            //
    "xhunter",           // Wellbia XIGNCODE3
    "xigncode",          //
    "mhyprot",           // miHoYo
    "vgk",               // Riot Vanguard
    "vgc",               //
    "sgguard",           // Sungard / NetEase
};

constexpr std::array<const char*, 8> kAntiCheatProcesses = {
    "easyanticheat", "easyanticheat_eos", "beservice", "bedaisy",
    "vgc",           "vgtray",            "ace-base",  "gameguard",
};

std::string lowered(const std::string& text) {
    std::string out = text;
    std::transform(out.begin(), out.end(), out.begin(),
                   [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    return out;
}

std::string file_name_of(const std::string& path) {
    const size_t slash = path.find_last_of("\\/");
    return slash == std::string::npos ? path : path.substr(slash + 1);
}

std::string strip_extension(const std::string& name) {
    const size_t dot = name.find_last_of('.');
    return dot == std::string::npos ? name : name.substr(0, dot);
}

// True when the process runs with a higher integrity level than this one. The
// hook cannot load into it, and trying looks exactly like an attack.
bool process_is_elevated(HANDLE process) {
    HANDLE token = nullptr;
    if (!OpenProcessToken(process, TOKEN_QUERY, &token)) return false;
    TOKEN_ELEVATION elevation{};
    DWORD size = 0;
    bool elevated = false;
    if (GetTokenInformation(token, TokenElevation, &elevation, sizeof(elevation), &size)) {
        elevated = elevation.TokenIsElevated != 0;
    }
    CloseHandle(token);

    if (!elevated) return false;

    // m3llo elevated as well is not a mismatch.
    HANDLE self_token = nullptr;
    bool self_elevated = false;
    if (OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &self_token)) {
        TOKEN_ELEVATION self{};
        if (GetTokenInformation(self_token, TokenElevation, &self, sizeof(self), &size)) {
            self_elevated = self.TokenIsElevated != 0;
        }
        CloseHandle(self_token);
    }
    return !self_elevated;
}

std::string executable_path_of(HANDLE process) {
    wchar_t path[MAX_PATH]{};
    DWORD size = MAX_PATH;
    if (!QueryFullProcessImageNameW(process, 0, path, &size)) return {};
    const int bytes = WideCharToMultiByte(CP_UTF8, 0, path, -1, nullptr, 0, nullptr, nullptr);
    if (bytes <= 1) return {};
    std::string out(static_cast<size_t>(bytes - 1), '\0');
    WideCharToMultiByte(CP_UTF8, 0, path, -1, out.data(), bytes, nullptr, nullptr);
    return out;
}

// Reads the loaded module list. A denial here is itself a reason to stop.
bool find_anticheat_module(HANDLE process, std::string* out_name) {
    std::array<HMODULE, 1024> modules{};
    DWORD needed = 0;
    if (!EnumProcessModulesEx(process, modules.data(),
                              static_cast<DWORD>(modules.size() * sizeof(HMODULE)), &needed,
                              LIST_MODULES_ALL)) {
        return false;
    }
    const size_t count = std::min<size_t>(modules.size(), needed / sizeof(HMODULE));
    for (size_t i = 0; i < count; ++i) {
        char name[MAX_PATH]{};
        if (GetModuleBaseNameA(process, modules[i], name, MAX_PATH) == 0) continue;
        if (is_anticheat_module(name)) {
            *out_name = name;
            return true;
        }
    }
    return false;
}

bool find_anticheat_process(std::string* out_name) {
    const HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snapshot == INVALID_HANDLE_VALUE) return false;

    PROCESSENTRY32W entry{};
    entry.dwSize = sizeof(entry);
    bool found = false;
    if (Process32FirstW(snapshot, &entry)) {
        do {
            char name[MAX_PATH]{};
            WideCharToMultiByte(CP_UTF8, 0, entry.szExeFile, -1, name, MAX_PATH, nullptr, nullptr);
            if (is_anticheat_process(name)) {
                *out_name = name;
                found = true;
                break;
            }
        } while (Process32NextW(snapshot, &entry));
    }
    CloseHandle(snapshot);
    return found;
}

bool window_class_is_chromium(uint32_t pid) {
    struct Search {
        uint32_t pid;
        bool     chromium = false;
    } search{pid};

    EnumWindows(
        [](HWND window, LPARAM param) -> BOOL {
            auto* s = reinterpret_cast<Search*>(param);
            DWORD owner = 0;
            GetWindowThreadProcessId(window, &owner);
            if (owner != s->pid || !IsWindowVisible(window)) return TRUE;
            char name[128]{};
            if (GetClassNameA(window, name, sizeof(name)) == 0) return TRUE;
            if (is_chromium_window_class(name)) {
                s->chromium = true;
                return FALSE;
            }
            return TRUE;
        },
        reinterpret_cast<LPARAM>(&search));
    return search.chromium;
}

} // namespace

const char* verdict_name(PolicyVerdict verdict) {
    switch (verdict) {
        case PolicyVerdict::Allowed:            return "allowed";
        case PolicyVerdict::NotAllowedByCaller: return "not on the safe list";
        case PolicyVerdict::ProcessUnreadable:  return "the process cannot be read";
        case PolicyVerdict::AntiCheatModule:    return "an anti-cheat module is loaded";
        case PolicyVerdict::AntiCheatProcess:   return "an anti-cheat service is running";
        case PolicyVerdict::StorePackaged:      return "a Store-packaged game";
        case PolicyVerdict::Chromium:           return "a Chromium shell";
        case PolicyVerdict::Elevated:           return "the game runs elevated";
    }
    return "unknown";
}

// The patterns match the start of the file name, not any part of it. These
// products all name their modules by a fixed prefix, and a search anywhere in
// the name turns short patterns such as "eac" into a refusal for any innocent
// module that happens to contain those letters.
bool matches_prefix(const std::string& name, const char* const* patterns, size_t count) {
    if (name.empty()) return false;
    for (size_t i = 0; i < count; ++i) {
        if (name.rfind(patterns[i], 0) == 0) return true;
    }
    return false;
}

bool is_anticheat_module(const std::string& module_name) {
    const std::string name = lowered(strip_extension(file_name_of(module_name)));
    return matches_prefix(name, kAntiCheatModules.data(), kAntiCheatModules.size());
}

bool is_anticheat_process(const std::string& process_name) {
    const std::string name = lowered(strip_extension(file_name_of(process_name)));
    return matches_prefix(name, kAntiCheatProcesses.data(), kAntiCheatProcesses.size());
}

bool is_store_packaged(const std::string& executable_path) {
    return lowered(executable_path).find("\\windowsapps\\") != std::string::npos;
}

bool is_chromium_window_class(const std::string& window_class) {
    return lowered(window_class).rfind("chrome_widgetwin", 0) == 0;
}

bool developer_allows(const std::string& variable_value, const std::string& executable_path) {
    if (variable_value.empty() || executable_path.empty()) return false;
    // One name, matched whole. A partial match would widen the allowance to
    // games the person setting this never meant to include.
    return lowered(file_name_of(variable_value)) == lowered(file_name_of(executable_path));
}

PolicyResult check_process(uint32_t pid, bool allowed_by_caller) {
    PolicyResult result;

    // The same rights the hook needs to be sure of what it is looking at. A
    // refusal here is the first sign of a protected process.
    const HANDLE process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, FALSE, pid);
    if (!process) {
        result.verdict = allowed_by_caller ? PolicyVerdict::ProcessUnreadable
                                           : PolicyVerdict::NotAllowedByCaller;
        if (allowed_by_caller) result.detail = std::to_string(GetLastError());
        return result;
    }

    struct Closer {
        HANDLE handle;
        ~Closer() { CloseHandle(handle); }
    } closer{process};

    const std::string path = executable_path_of(process);
    if (!allowed_by_caller) {
        char value[MAX_PATH]{};
        const DWORD length = GetEnvironmentVariableA(kDeveloperAllowVariable, value, MAX_PATH);
        const std::string variable = (length > 0 && length < MAX_PATH) ? value : "";
        if (!developer_allows(variable, path)) {
            result.verdict = PolicyVerdict::NotAllowedByCaller;
            return result;
        }
        MELLO_LOG_WARN(TAG,
                       "%s allows the hook for %s. This is a developer setting and it stands in "
                       "for the backend safe list.",
                       kDeveloperAllowVariable, file_name_of(path).c_str());
    }

    if (process_is_elevated(process)) {
        result.verdict = PolicyVerdict::Elevated;
        return result;
    }

    if (is_store_packaged(path)) {
        result.verdict = PolicyVerdict::StorePackaged;
        result.detail = path;
        return result;
    }

    std::string module_name;
    if (find_anticheat_module(process, &module_name)) {
        result.verdict = PolicyVerdict::AntiCheatModule;
        result.detail = module_name;
        return result;
    }

    std::string process_name;
    if (find_anticheat_process(&process_name)) {
        result.verdict = PolicyVerdict::AntiCheatProcess;
        result.detail = process_name;
        return result;
    }

    if (window_class_is_chromium(pid)) {
        result.verdict = PolicyVerdict::Chromium;
        return result;
    }

    result.verdict = PolicyVerdict::Allowed;
    MELLO_LOG_INFO(TAG, "hook allowed for pid=%u (%s)", pid, file_name_of(path).c_str());
    return result;
}

} // namespace mello::video::hook

#endif // _WIN32
