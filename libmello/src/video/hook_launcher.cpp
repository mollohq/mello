#include "hook_launcher.hpp"

#ifdef _WIN32

#include <windows.h>

#include <array>
#include <cstdlib>
#include <mutex>
#include <string>
#include <unordered_map>

#include "../util/log.hpp"

namespace mello::video::hook {
namespace {

constexpr const char* TAG = "video/hook";

// A helper that takes longer than this has nothing useful to say. The offsets
// helper builds one device; the injection helper has its own deadline.
constexpr DWORD kOffsetsTimeoutMs = 10'000;

std::string directory_of_this_process() {
    wchar_t path[MAX_PATH]{};
    if (GetModuleFileNameW(nullptr, path, MAX_PATH) == 0) return {};
    std::wstring wide(path);
    const size_t slash = wide.find_last_of(L'\\');
    if (slash == std::wstring::npos) return {};
    wide.resize(slash);

    const int size = WideCharToMultiByte(CP_UTF8, 0, wide.c_str(), -1, nullptr, 0, nullptr, nullptr);
    if (size <= 1) return {};
    std::string out(static_cast<size_t>(size - 1), '\0');
    WideCharToMultiByte(CP_UTF8, 0, wide.c_str(), -1, out.data(), size, nullptr, nullptr);
    return out;
}

bool file_exists(const std::string& path) {
    const DWORD attributes = GetFileAttributesA(path.c_str());
    return attributes != INVALID_FILE_ATTRIBUTES && !(attributes & FILE_ATTRIBUTE_DIRECTORY);
}

std::wstring widen(const std::string& text) {
    if (text.empty()) return {};
    const int size = MultiByteToWideChar(CP_UTF8, 0, text.c_str(), -1, nullptr, 0);
    if (size <= 1) return {};
    std::wstring out(static_cast<size_t>(size - 1), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, text.c_str(), -1, out.data(), size);
    return out;
}

// Runs `command_line` and returns its exit code. `stdout_text`, when given,
// receives what the child printed. Every wait is bounded: a helper that hangs
// must never hold up a stream.
bool run_helper(const std::string& command_line, DWORD timeout_ms, DWORD* out_exit_code,
                std::string* stdout_text) {
    HANDLE read_end = nullptr;
    HANDLE write_end = nullptr;
    SECURITY_ATTRIBUTES sa{};
    sa.nLength = sizeof(sa);
    sa.bInheritHandle = TRUE;

    if (stdout_text) {
        if (!CreatePipe(&read_end, &write_end, &sa, 0)) return false;
        SetHandleInformation(read_end, HANDLE_FLAG_INHERIT, 0);
    }

    STARTUPINFOW si{};
    si.cb = sizeof(si);
    if (stdout_text) {
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdOutput = write_end;
        si.hStdError = write_end;
    }
    PROCESS_INFORMATION pi{};

    std::wstring wide = widen(command_line);
    wide.push_back(L'\0');
    const BOOL started = CreateProcessW(nullptr, wide.data(), nullptr, nullptr,
                                        stdout_text ? TRUE : FALSE, CREATE_NO_WINDOW, nullptr,
                                        nullptr, &si, &pi);
    if (write_end) CloseHandle(write_end);
    if (!started) {
        if (read_end) CloseHandle(read_end);
        MELLO_LOG_ERROR(TAG, "cannot start helper: %lu", GetLastError());
        return false;
    }

    if (stdout_text) {
        std::array<char, 512> buffer{};
        DWORD read = 0;
        while (ReadFile(read_end, buffer.data(), static_cast<DWORD>(buffer.size()), &read,
                        nullptr) &&
               read > 0) {
            stdout_text->append(buffer.data(), read);
        }
        CloseHandle(read_end);
    }

    const DWORD wait = WaitForSingleObject(pi.hProcess, timeout_ms);
    DWORD exit_code = 0xFFFFFFFF;
    if (wait == WAIT_OBJECT_0) {
        GetExitCodeProcess(pi.hProcess, &exit_code);
    } else {
        MELLO_LOG_ERROR(TAG, "helper did not finish within %lu ms; killing it", timeout_ms);
        TerminateProcess(pi.hProcess, 1);
        WaitForSingleObject(pi.hProcess, 1000);
    }
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);

    if (out_exit_code) *out_exit_code = exit_code;
    return wait == WAIT_OBJECT_0;
}

uint64_t parse_hex(const std::string& text) {
    return static_cast<uint64_t>(std::strtoull(text.c_str(), nullptr, 0));
}

// The helper prints `key=value` lines. Anything else is ignored, so a new key
// in a later version of the helper cannot break an older client.
Offsets parse_offsets(const std::string& text) {
    Offsets out;
    size_t start = 0;
    while (start < text.size()) {
        size_t end = text.find('\n', start);
        if (end == std::string::npos) end = text.size();
        std::string line = text.substr(start, end - start);
        start = end + 1;
        while (!line.empty() && (line.back() == '\r' || line.back() == ' ')) line.pop_back();

        const size_t equals = line.find('=');
        if (equals == std::string::npos) continue;
        const std::string key = line.substr(0, equals);
        const std::string value = line.substr(equals + 1);

        if (key == "dxgi_file_version") {
            out.dxgi_file_version = value;
        } else if (key == "dxgi_present") {
            out.dxgi_present = parse_hex(value);
        } else if (key == "dxgi_present1") {
            out.dxgi_present1 = parse_hex(value);
        } else if (key == "dxgi_resize_buffers") {
            out.dxgi_resize_buffers = parse_hex(value);
        } else if (key == "d3d9_file_version") {
            out.d3d9_file_version = value;
        } else if (key == "d3d9_present") {
            out.d3d9_present = parse_hex(value);
        } else if (key == "d3d9_present_ex") {
            out.d3d9_present_ex = parse_hex(value);
        } else if (key == "d3d9_swapchain_present") {
            out.d3d9_swapchain_present = parse_hex(value);
        } else if (key == "d3d9_reset") {
            out.d3d9_reset = parse_hex(value);
        } else if (key == "d3d9_reset_ex") {
            out.d3d9_reset_ex = parse_hex(value);
        }
    }
    // One present function is enough: the game uses one graphics API, and the
    // hook takes whichever of them is loaded in it.
    out.valid = out.dxgi_present != 0 || out.d3d9_present != 0;
    return out;
}

std::mutex                              g_offsets_mutex;
std::unordered_map<int, Offsets>        g_offsets_cache;

} // namespace

std::string hook_directory() {
    // A developer build points this at hook/build/<arch>/<config>. An install
    // keeps the helpers next to the client.
    char buffer[MAX_PATH]{};
    const DWORD length = GetEnvironmentVariableA("MELLO_HOOK_DIR", buffer, MAX_PATH);
    if (length > 0 && length < MAX_PATH) return buffer;
    return directory_of_this_process();
}

const Offsets& offsets_for(int bits) {
    std::lock_guard<std::mutex> lock(g_offsets_mutex);
    auto cached = g_offsets_cache.find(bits);
    if (cached != g_offsets_cache.end()) return cached->second;

    Offsets offsets;
    const std::string directory = hook_directory();
    const std::string helper = directory + "\\mello-offsets" + std::to_string(bits) + ".exe";
    if (!file_exists(helper)) {
        MELLO_LOG_WARN(TAG, "offsets helper not found: %s", helper.c_str());
        return g_offsets_cache.emplace(bits, offsets).first->second;
    }

    std::string output;
    DWORD exit_code = 0;
    if (!run_helper("\"" + helper + "\"", kOffsetsTimeoutMs, &exit_code, &output) ||
        exit_code != 0) {
        MELLO_LOG_WARN(TAG, "offsets helper (%d-bit) failed: exit=%lu", bits, exit_code);
        return g_offsets_cache.emplace(bits, offsets).first->second;
    }

    offsets = parse_offsets(output);
    if (offsets.valid) {
        MELLO_LOG_INFO(TAG,
                       "present offsets for %d-bit games: dxgi.dll %s present=0x%llx "
                       "present1=0x%llx resize=0x%llx; d3d9.dll %s present=0x%llx "
                       "present_ex=0x%llx",
                       bits, offsets.dxgi_file_version.c_str(),
                       static_cast<unsigned long long>(offsets.dxgi_present),
                       static_cast<unsigned long long>(offsets.dxgi_present1),
                       static_cast<unsigned long long>(offsets.dxgi_resize_buffers),
                       offsets.d3d9_file_version.c_str(),
                       static_cast<unsigned long long>(offsets.d3d9_present),
                       static_cast<unsigned long long>(offsets.d3d9_present_ex));
    } else {
        MELLO_LOG_WARN(TAG, "offsets helper printed nothing usable");
    }
    return g_offsets_cache.emplace(bits, offsets).first->second;
}

const char* inject_result_name(InjectResult result) {
    switch (result) {
        case InjectResult::Ready:          return "ready";
        case InjectResult::HelperMissing:  return "helper missing";
        case InjectResult::HelperFailed:   return "helper failed";
        case InjectResult::NoWindowThread: return "no window thread";
        case InjectResult::Timeout:        return "timeout";
    }
    return "unknown";
}

InjectResult inject(uint32_t pid, int bits, uint32_t timeout_ms) {
    const std::string directory = hook_directory();
    const std::string helper = directory + "\\mello-inject" + std::to_string(bits) + ".exe";
    if (!file_exists(helper)) {
        MELLO_LOG_WARN(TAG, "injection helper not found: %s", helper.c_str());
        return InjectResult::HelperMissing;
    }

    const std::string command = "\"" + helper + "\" " + std::to_string(pid) + " " +
                                std::to_string(timeout_ms);
    DWORD exit_code = 0;
    // The helper has its own deadline. This one only catches a helper that
    // never returns at all.
    if (!run_helper(command, timeout_ms + 2000, &exit_code, nullptr)) {
        MELLO_LOG_WARN(TAG, "injection helper for pid=%u did not finish", pid);
        return InjectResult::HelperFailed;
    }
    switch (exit_code) {
        case 0: return InjectResult::Ready;
        case 2: return InjectResult::Timeout;
        case 3: return InjectResult::NoWindowThread;
        default:
            MELLO_LOG_WARN(TAG, "injection helper for pid=%u exited with %lu", pid, exit_code);
            return InjectResult::HelperFailed;
    }
}

} // namespace mello::video::hook

#endif // _WIN32
