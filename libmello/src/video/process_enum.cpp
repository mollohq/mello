#include "process_enum.hpp"
#include "../util/log.hpp"

#ifdef _WIN32
#include "capture_process.hpp"
#include <Windows.h>
#include <dwmapi.h>
#include <dxgi1_2.h>
#include <TlHelp32.h>
#include <wrl/client.h>
#include <algorithm>
#include <cctype>
#include <unordered_map>
#pragma comment(lib, "dwmapi.lib")
#pragma comment(lib, "dxgi.lib")
using Microsoft::WRL::ComPtr;
#endif

#if defined(__linux__)
#include <climits>
#include <dirent.h>
#include <unistd.h>
#include <cctype>
#include <cstdlib>
#include <fstream>
#include <sstream>
#endif

namespace mello::video {

static constexpr const char* TAG = "video/process";

namespace detail {

std::string path_basename(const std::string& path) {
    const size_t pos = path.find_last_of("/\\");
    return pos == std::string::npos ? path : path.substr(pos + 1);
}

#if defined(__linux__)
std::string wine_path_to_host(const std::string& windows_path, const std::string& wine_prefix) {
    std::string path = windows_path;
    if (path.rfind("\\\\?\\", 0) == 0) path = path.substr(4);
    if (path.size() < 3 || !std::isalpha(static_cast<unsigned char>(path[0])) ||
        path[1] != ':' || path[2] != '\\') {
        return {};
    }
    const char drive = static_cast<char>(std::tolower(static_cast<unsigned char>(path[0])));

    std::string root;
    if (!wine_prefix.empty()) {
        const std::string link = wine_prefix + "/dosdevices/" + drive + ":";
        char resolved[PATH_MAX];
        if (realpath(link.c_str(), resolved)) root = resolved;
    }
    if (root.empty() && drive == 'z') root = "/";
    if (root.empty()) return {};

    std::string rest = path.substr(3);
    for (char& c : rest) {
        if (c == '\\') c = '/';
    }
    if (root.back() != '/') root += '/';
    return root + rest;
}
#endif

} // namespace detail

#ifdef _WIN32

std::vector<MonitorInfo> enumerate_monitors() {
    std::vector<MonitorInfo> result;

    ComPtr<IDXGIFactory1> factory;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), &factory))) {
        MELLO_LOG_ERROR(TAG, "CreateDXGIFactory1 failed");
        return result;
    }

    ComPtr<IDXGIAdapter1> adapter;
    for (UINT ai = 0; factory->EnumAdapters1(ai, &adapter) == S_OK; ++ai) {
        ComPtr<IDXGIOutput> output;
        for (UINT oi = 0; adapter->EnumOutputs(oi, &output) == S_OK; ++oi) {
            DXGI_OUTPUT_DESC desc{};
            if (SUCCEEDED(output->GetDesc(&desc))) {
                uint32_t w = desc.DesktopCoordinates.right  - desc.DesktopCoordinates.left;
                uint32_t h = desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top;

                char name_utf8[128]{};
                WideCharToMultiByte(CP_UTF8, 0, desc.DeviceName, -1,
                                    name_utf8, sizeof(name_utf8), nullptr, nullptr);

                MonitorInfo mi;
                mi.index   = static_cast<uint32_t>(result.size());
                mi.name    = name_utf8;
                mi.width   = w;
                mi.height  = h;
                mi.primary = (desc.DesktopCoordinates.left == 0 &&
                              desc.DesktopCoordinates.top  == 0);
                result.push_back(std::move(mi));
            }
            output.Reset();
        }
        adapter.Reset();
    }

    MELLO_LOG_DEBUG(TAG, "Enumerated %zu monitors", result.size());
    return result;
}

/// True when the window covers its monitor's full area — matches both
/// borderless-windowed and exclusive fullscreen. Deliberately avoids the DXGI
/// probe in capture_process.cpp, which needs a live D3D11 device and only
/// detects exclusive mode.
static bool window_is_fullscreen(HWND hwnd) {
    RECT wr{};
    if (!GetWindowRect(hwnd, &wr)) return false;
    HMONITOR mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    MONITORINFO mi{};
    mi.cbSize = sizeof(mi);
    if (!GetMonitorInfoW(mon, &mi)) return false;
    return wr.left <= mi.rcMonitor.left && wr.top <= mi.rcMonitor.top &&
           wr.right >= mi.rcMonitor.right && wr.bottom >= mi.rcMonitor.bottom;
}

/// Process creation time as Unix epoch milliseconds, or 0 when unavailable.
///
/// FILETIME counts 100ns intervals from 1601-01-01; the Unix epoch sits
/// 11644473600 seconds later. Opening with PROCESS_QUERY_LIMITED_INFORMATION
/// succeeds for normal user processes without elevation; protected and system
/// processes fail, which is why callers must tolerate 0.
static int64_t process_started_at_ms(uint32_t pid) {
    HANDLE proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
    if (!proc) return 0;
    FILETIME created{}, exited{}, kernel{}, user{};
    int64_t out = 0;
    if (GetProcessTimes(proc, &created, &exited, &kernel, &user)) {
        ULARGE_INTEGER t{};
        t.LowPart  = created.dwLowDateTime;
        t.HighPart = created.dwHighDateTime;
        // Guard against a clock that predates the Unix epoch rather than
        // wrapping into a huge negative timestamp.
        if (t.QuadPart > 116444736000000000ULL) {
            out = static_cast<int64_t>((t.QuadPart - 116444736000000000ULL) / 10000ULL);
        }
    }
    CloseHandle(proc);
    return out;
}

std::vector<GameProcess> enumerate_game_processes() {
    std::vector<GameProcess> result;

    // One window pass supplies per-pid title/path/fullscreen; the snapshot
    // below still lists every process (windowless processes keep empty
    // window fields). EnumWindows returns in z-order, so the first window
    // seen for a pid is its topmost — treat it as the main window.
    auto windows = enumerate_visible_windows();
    std::unordered_map<uint32_t, const VisibleWindow*> window_by_pid;
    window_by_pid.reserve(windows.size());
    for (const auto& w : windows) {
        window_by_pid.try_emplace(w.pid, &w);
    }

    DWORD fg_pid = 0;
    if (HWND fg = GetForegroundWindow()) {
        GetWindowThreadProcessId(fg, &fg_pid);
    }

    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snap == INVALID_HANDLE_VALUE) {
        MELLO_LOG_ERROR(TAG, "CreateToolhelp32Snapshot failed");
        return result;
    }

    PROCESSENTRY32W pe{};
    pe.dwSize = sizeof(pe);

    if (Process32FirstW(snap, &pe)) {
        do {
            char exe_name[260]{};
            WideCharToMultiByte(CP_UTF8, 0, pe.szExeFile, -1, exe_name, sizeof(exe_name), nullptr, nullptr);

            GameProcess gp;
            gp.pid  = pe.th32ProcessID;
            gp.name = exe_name;
            gp.exe  = exe_name;
            gp.is_fullscreen = false;
            gp.is_foreground = (fg_pid != 0 && gp.pid == fg_pid);
            gp.started_at_ms = process_started_at_ms(gp.pid);
            if (auto it = window_by_pid.find(gp.pid); it != window_by_pid.end()) {
                const VisibleWindow* w = it->second;
                gp.window_title  = w->title;
                gp.path          = w->path;
                gp.is_fullscreen = window_is_fullscreen(static_cast<HWND>(w->hwnd));
            }
            result.push_back(std::move(gp));
        } while (Process32NextW(snap, &pe));
    }

    CloseHandle(snap);

    MELLO_LOG_DEBUG(TAG, "Enumerated %zu processes", result.size());
    return result;
}

static BOOL CALLBACK enum_windows_cb(HWND hwnd, LPARAM lparam) {
    auto* result = reinterpret_cast<std::vector<VisibleWindow>*>(lparam);

    if (!IsWindowVisible(hwnd)) return TRUE;
    if (hwnd == GetDesktopWindow()) return TRUE;
    if (hwnd == GetShellWindow()) return TRUE;

    // Skip tool windows (floating toolbars, tooltips, etc.)
    LONG_PTR ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    if (ex_style & WS_EX_TOOLWINDOW) return TRUE;

    // Skip cloaked UWP windows (hidden Store apps, etc.)
    BOOL cloaked = FALSE;
    DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &cloaked, sizeof(cloaked));
    if (cloaked) return TRUE;

    int title_len = GetWindowTextLengthW(hwnd);
    if (title_len <= 0) return TRUE;

    std::wstring wtitle(title_len + 1, L'\0');
    GetWindowTextW(hwnd, wtitle.data(), title_len + 1);

    char title_utf8[256]{};
    WideCharToMultiByte(CP_UTF8, 0, wtitle.c_str(), -1, title_utf8, sizeof(title_utf8), nullptr, nullptr);

    DWORD pid = 0;
    GetWindowThreadProcessId(hwnd, &pid);

    std::string exe_name;
    std::string full_path;
    if (HANDLE proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid)) {
        wchar_t exe_path[MAX_PATH]{};
        DWORD path_len = MAX_PATH;
        if (QueryFullProcessImageNameW(proc, 0, exe_path, &path_len)) {
            char path_utf8[520]{};
            WideCharToMultiByte(CP_UTF8, 0, exe_path, -1, path_utf8, sizeof(path_utf8), nullptr, nullptr);
            full_path = path_utf8;
            const wchar_t* slash = wcsrchr(exe_path, L'\\');
            const wchar_t* fname = slash ? slash + 1 : exe_path;
            char fname_utf8[256]{};
            WideCharToMultiByte(CP_UTF8, 0, fname, -1, fname_utf8, sizeof(fname_utf8), nullptr, nullptr);
            exe_name = fname_utf8;
        }
        CloseHandle(proc);
    }

    VisibleWindow vw;
    vw.hwnd  = hwnd;
    vw.title = title_utf8;
    vw.exe   = std::move(exe_name);
    vw.path  = std::move(full_path);
    vw.pid   = pid;
    result->push_back(std::move(vw));

    return TRUE;
}

std::vector<VisibleWindow> enumerate_visible_windows() {
    std::vector<VisibleWindow> result;
    EnumWindows(enum_windows_cb, reinterpret_cast<LPARAM>(&result));
    MELLO_LOG_DEBUG(TAG, "Enumerated %zu visible windows", result.size());
    return result;
}

#else

std::vector<MonitorInfo> enumerate_monitors() {
    MELLO_LOG_WARN(TAG, "Monitor enumeration not supported on this platform");
    return {};
}

#if defined(__linux__)

namespace {

std::string read_file(const std::string& path) {
    std::ifstream f(path, std::ios::binary);
    if (!f) return {};
    std::ostringstream ss;
    ss << f.rdbuf();
    return ss.str();
}

/// Boot time, Unix seconds (/proc/stat "btime"). 0 when unreadable.
int64_t boot_time_s() {
    std::istringstream in(read_file("/proc/stat"));
    std::string key;
    while (in >> key) {
        if (key == "btime") {
            int64_t t = 0;
            in >> t;
            return t;
        }
        in.ignore(4096, '\n');
    }
    return 0;
}

/// Start time of a process in Unix ms, from field 22 of /proc/<pid>/stat
/// (clock ticks after boot). 0 when unavailable.
int64_t start_time_ms(const std::string& pid_dir, int64_t boot_s, long ticks_per_s) {
    const std::string stat = read_file(pid_dir + "/stat");
    // The command name (field 2) may hold spaces and parentheses: the fields
    // after it start after its last ')'.
    const size_t close = stat.rfind(')');
    if (close == std::string::npos || boot_s == 0 || ticks_per_s <= 0) return 0;
    std::istringstream in(stat.substr(close + 1));
    std::string field;
    // Fields 3..21 come before starttime (22).
    for (int i = 3; i <= 21; ++i) {
        if (!(in >> field)) return 0;
    }
    unsigned long long ticks = 0;
    if (!(in >> ticks)) return 0;
    return boot_s * 1000 + static_cast<int64_t>(ticks * 1000ULL / static_cast<unsigned long long>(ticks_per_s));
}

bool ends_with_exe(const std::string& name) {
    if (name.size() < 4) return false;
    std::string tail = name.substr(name.size() - 4);
    for (char& c : tail) c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
    return tail == ".exe";
}

/// The Wine prefix of a process: WINEPREFIX, else Proton's
/// STEAM_COMPAT_DATA_PATH/pfx. Empty when neither is set or the environment
/// is not readable.
std::string wine_prefix_of(const std::string& pid_dir) {
    const std::string env = read_file(pid_dir + "/environ");
    std::string compat;
    size_t start = 0;
    while (start < env.size()) {
        size_t end = env.find('\0', start);
        if (end == std::string::npos) end = env.size();
        const std::string var = env.substr(start, end - start);
        if (var.rfind("WINEPREFIX=", 0) == 0) return var.substr(11);
        if (var.rfind("STEAM_COMPAT_DATA_PATH=", 0) == 0) compat = var.substr(23);
        start = end + 1;
    }
    return compat.empty() ? std::string() : compat + "/pfx";
}

bool is_wine_loader(const std::string& exe) {
    return exe == "wine-preloader" || exe == "wine64-preloader" || exe == "wine" ||
           exe == "wine64";
}

} // namespace

std::vector<GameProcess> enumerate_game_processes() {
    std::vector<GameProcess> result;
    DIR* proc = opendir("/proc");
    if (!proc) {
        MELLO_LOG_ERROR(TAG, "cannot open /proc");
        return result;
    }
    const int64_t boot_s = boot_time_s();
    const long ticks_per_s = sysconf(_SC_CLK_TCK);

    while (dirent* entry = readdir(proc)) {
        const char* name = entry->d_name;
        if (!std::isdigit(static_cast<unsigned char>(name[0]))) continue;
        char* end = nullptr;
        const unsigned long pid = std::strtoul(name, &end, 10);
        if (!end || *end != '\0') continue;
        const std::string dir = std::string("/proc/") + name;

        // The exe link is readable for the user's own processes only.
        char exe_buf[4096];
        const ssize_t n = readlink((dir + "/exe").c_str(), exe_buf, sizeof(exe_buf) - 1);
        if (n <= 0) continue;
        std::string exe_path(exe_buf, static_cast<size_t>(n));
        const std::string deleted = " (deleted)";
        if (exe_path.size() > deleted.size() &&
            exe_path.compare(exe_path.size() - deleted.size(), deleted.size(), deleted) == 0) {
            exe_path.resize(exe_path.size() - deleted.size());
        }

        GameProcess gp{};
        gp.pid = static_cast<uint32_t>(pid);
        gp.exe = detail::path_basename(exe_path);
        gp.path = exe_path;

        // Wine and Proton: /proc/<pid>/exe is the preloader; argv0 is the
        // Windows program, often padded with spaces.
        const std::string cmdline = read_file(dir + "/cmdline");
        std::string argv0 = cmdline.substr(0, cmdline.find('\0'));
        while (!argv0.empty() && argv0.back() == ' ') argv0.pop_back();
        if (ends_with_exe(argv0) && (is_wine_loader(gp.exe) || argv0.find('\\') != std::string::npos)) {
            gp.exe = detail::path_basename(argv0);
            gp.path = detail::wine_path_to_host(argv0, wine_prefix_of(dir));
        }

        gp.name = gp.exe;
        gp.started_at_ms = start_time_ms(dir, boot_s, ticks_per_s);
        result.push_back(std::move(gp));
    }
    closedir(proc);
    return result;
}

#else

std::vector<GameProcess> enumerate_game_processes() {
    MELLO_LOG_WARN(TAG, "Game process enumeration not supported on this platform");
    return {};
}

#endif

std::vector<VisibleWindow> enumerate_visible_windows() {
    MELLO_LOG_WARN(TAG, "Window enumeration not supported on this platform");
    return {};
}

#endif

} // namespace mello::video
