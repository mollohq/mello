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

namespace mello::video {

static constexpr const char* TAG = "video/process";

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

std::vector<GameProcess> enumerate_game_processes() {
    MELLO_LOG_WARN(TAG, "Game process enumeration not supported on this platform");
    return {};
}

std::vector<VisibleWindow> enumerate_visible_windows() {
    MELLO_LOG_WARN(TAG, "Window enumeration not supported on this platform");
    return {};
}

#endif

} // namespace mello::video
