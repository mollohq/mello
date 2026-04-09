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

std::vector<GameProcess> enumerate_game_processes() {
    std::vector<GameProcess> result;

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
    if (HANDLE proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid)) {
        wchar_t exe_path[MAX_PATH]{};
        DWORD path_len = MAX_PATH;
        if (QueryFullProcessImageNameW(proc, 0, exe_path, &path_len)) {
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
