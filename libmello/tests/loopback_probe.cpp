// Game clips (CLIPS.md 15.12): measure WASAPI
// process loopback on a running game before the replay ring depends on it.
//
// Opens the stream host's loopback (WasapiLoopbackCapture, include the target
// process tree) on a pid and logs one line per second: the level of each
// channel, the share of digital zero samples, the frames that arrived, and the
// game's own session in the Windows volume mixer (slider, mute, peak meter).
// Endpoint loopback is refused: it is not a fallback for the replay.
//
//   mello_loopback_probe --exe cs2.exe --seconds 600 --csv out.csv [--wav out.wav]
//   mello_loopback_probe --pid 1234 ...
//   ... --slider 10:0.5,20:1   set the target's mixer slider at 10 s and 20 s
//
// Windows only. Not a test: nothing runs it in ctest.

#include "audio/capture_wasapi_loopback.hpp"

#include <audioclient.h>
#include <audiopolicy.h>
#include <endpointvolume.h>
#include <mmdeviceapi.h>
#include <tlhelp32.h>
#include <windows.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <set>
#include <string>
#include <thread>
#include <vector>

namespace {

struct Bucket {
    double sum_sq_l = 0.0;
    double sum_sq_r = 0.0;
    int32_t peak = 0;
    uint64_t zero_samples = 0;
    uint64_t samples = 0;
};

struct Session {
    bool found = false;
    float volume = -1.0f;
    bool muted = false;
    float peak = -1.0f;
};

double to_db(double linear) {
    return linear > 1e-9 ? 20.0 * std::log10(linear) : -120.0;
}

std::wstring widen(const char* s) {
    std::wstring out;
    while (*s) out.push_back(static_cast<wchar_t>(static_cast<unsigned char>(*s++)));
    return out;
}

/// Every process whose executable is `exe` (case-insensitive).
std::vector<uint32_t> pids_of(const char* exe) {
    std::vector<uint32_t> out;
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snap == INVALID_HANDLE_VALUE) return out;
    PROCESSENTRY32W pe{};
    pe.dwSize = sizeof(pe);
    const std::wstring want = widen(exe);
    for (BOOL ok = Process32FirstW(snap, &pe); ok; ok = Process32NextW(snap, &pe)) {
        if (_wcsicmp(pe.szExeFile, want.c_str()) == 0) out.push_back(pe.th32ProcessID);
    }
    CloseHandle(snap);
    return out;
}

/// `root` and every process below it, by parent pid.
std::set<uint32_t> process_tree(uint32_t root) {
    std::set<uint32_t> tree{root};
    std::vector<std::pair<uint32_t, uint32_t>> edges;  // pid, parent
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snap == INVALID_HANDLE_VALUE) return tree;
    PROCESSENTRY32W pe{};
    pe.dwSize = sizeof(pe);
    for (BOOL ok = Process32FirstW(snap, &pe); ok; ok = Process32NextW(snap, &pe)) {
        edges.emplace_back(pe.th32ProcessID, pe.th32ParentProcessID);
    }
    CloseHandle(snap);
    for (bool grew = true; grew;) {
        grew = false;
        for (const auto& [pid, parent] : edges) {
            if (tree.count(parent) && !tree.count(pid) && pid != parent) {
                tree.insert(pid);
                grew = true;
            }
        }
    }
    return tree;
}

/// The game's audio session on the default render endpoint: the volume mixer
/// slider, mute and the session peak meter. The first session of a process in
/// the tree wins.
Session read_session(IAudioSessionManager2* mgr, const std::set<uint32_t>& tree) {
    Session s{};
    if (!mgr) return s;
    IAudioSessionEnumerator* sessions = nullptr;
    if (FAILED(mgr->GetSessionEnumerator(&sessions))) return s;
    int count = 0;
    sessions->GetCount(&count);
    for (int i = 0; i < count && !s.found; ++i) {
        IAudioSessionControl* ctl = nullptr;
        if (FAILED(sessions->GetSession(i, &ctl))) continue;
        IAudioSessionControl2* ctl2 = nullptr;
        if (SUCCEEDED(ctl->QueryInterface(__uuidof(IAudioSessionControl2),
                                          reinterpret_cast<void**>(&ctl2)))) {
            DWORD pid = 0;
            ctl2->GetProcessId(&pid);
            if (tree.count(pid)) {
                s.found = true;
                ISimpleAudioVolume* vol = nullptr;
                if (SUCCEEDED(ctl->QueryInterface(__uuidof(ISimpleAudioVolume),
                                                  reinterpret_cast<void**>(&vol)))) {
                    BOOL mute = FALSE;
                    vol->GetMasterVolume(&s.volume);
                    vol->GetMute(&mute);
                    s.muted = mute != FALSE;
                    vol->Release();
                }
                IAudioMeterInformation* meter = nullptr;
                if (SUCCEEDED(ctl->QueryInterface(__uuidof(IAudioMeterInformation),
                                                  reinterpret_cast<void**>(&meter)))) {
                    meter->GetPeakValue(&s.peak);
                    meter->Release();
                }
            }
            ctl2->Release();
        }
        ctl->Release();
    }
    sessions->Release();
    return s;
}

/// Set the volume mixer slider of the target's session, as the volume mixer
/// does. Returns false when the tree has no session on the default endpoint.
bool set_session_volume(IAudioSessionManager2* mgr, const std::set<uint32_t>& tree, float volume) {
    if (!mgr) return false;
    IAudioSessionEnumerator* sessions = nullptr;
    if (FAILED(mgr->GetSessionEnumerator(&sessions))) return false;
    bool done = false;
    int count = 0;
    sessions->GetCount(&count);
    for (int i = 0; i < count && !done; ++i) {
        IAudioSessionControl* ctl = nullptr;
        if (FAILED(sessions->GetSession(i, &ctl))) continue;
        IAudioSessionControl2* ctl2 = nullptr;
        if (SUCCEEDED(ctl->QueryInterface(__uuidof(IAudioSessionControl2),
                                          reinterpret_cast<void**>(&ctl2)))) {
            DWORD pid = 0;
            ctl2->GetProcessId(&pid);
            ISimpleAudioVolume* vol = nullptr;
            if (tree.count(pid) && SUCCEEDED(ctl->QueryInterface(__uuidof(ISimpleAudioVolume),
                                                                 reinterpret_cast<void**>(&vol)))) {
                done = SUCCEEDED(vol->SetMasterVolume(volume, nullptr));
                vol->Release();
            }
            ctl2->Release();
        }
        ctl->Release();
    }
    sessions->Release();
    return done;
}

void write_wav_header(FILE* f, uint32_t data_bytes) {
    const uint32_t rate = 48000, byte_rate = rate * 4, fmt_size = 16, riff = data_bytes + 36;
    const uint16_t pcm = 1, channels = 2, align = 4, bits = 16;
    std::fseek(f, 0, SEEK_SET);
    std::fwrite("RIFF", 1, 4, f);
    std::fwrite(&riff, 4, 1, f);
    std::fwrite("WAVEfmt ", 1, 8, f);
    std::fwrite(&fmt_size, 4, 1, f);
    std::fwrite(&pcm, 2, 1, f);
    std::fwrite(&channels, 2, 1, f);
    std::fwrite(&rate, 4, 1, f);
    std::fwrite(&byte_rate, 4, 1, f);
    std::fwrite(&align, 2, 1, f);
    std::fwrite(&bits, 2, 1, f);
    std::fwrite("data", 1, 4, f);
    std::fwrite(&data_bytes, 4, 1, f);
}

const char* arg(int argc, char** argv, const char* name) {
    for (int i = 1; i + 1 < argc; ++i) {
        if (std::strcmp(argv[i], name) == 0) return argv[i + 1];
    }
    return nullptr;
}

}  // namespace

int main(int argc, char** argv) {
    const char* exe = arg(argc, argv, "--exe");
    const char* pid_arg = arg(argc, argv, "--pid");
    const char* seconds_arg = arg(argc, argv, "--seconds");
    const char* csv_path = arg(argc, argv, "--csv");
    const char* wav_path = arg(argc, argv, "--wav");
    const int seconds = seconds_arg ? std::atoi(seconds_arg) : 600;
    // "--slider 10:0.5,20:1" sets the target's mixer slider at those seconds.
    std::vector<std::pair<int, float>> slider_steps;
    if (const char* steps = arg(argc, argv, "--slider")) {
        for (const char* p = steps; *p;) {
            int at = 0;
            float value = 0.0f;
            if (std::sscanf(p, "%d:%f", &at, &value) == 2) slider_steps.emplace_back(at, value);
            const char* comma = std::strchr(p, ',');
            if (!comma) break;
            p = comma + 1;
        }
    }

    uint32_t pid = pid_arg ? static_cast<uint32_t>(std::strtoul(pid_arg, nullptr, 10)) : 0;
    if (!pid && exe) {
        const auto pids = pids_of(exe);
        if (pids.empty()) {
            std::fprintf(stderr, "no running process named %s\n", exe);
            return 2;
        }
        pid = pids.front();
        if (pids.size() > 1) {
            std::printf("%zu processes named %s, using pid %u\n", pids.size(), exe, pid);
        }
    }
    if (!pid) {
        std::fprintf(stderr, "usage: --exe NAME.exe | --pid N [--seconds S] [--csv F] [--wav F]\n");
        return 2;
    }

    CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);

    mello::audio::WasapiLoopbackCapture capture;
    if (!capture.initialize_scoped(mello::audio::LoopbackScope::IncludeProcessTree, pid) ||
        capture.scope() != mello::audio::LoopbackScope::IncludeProcessTree) {
        std::fprintf(stderr, "process loopback on pid %u did not start (endpoint loopback is refused)\n",
                     pid);
        return 1;
    }
    std::printf("process loopback on pid %u: %u Hz, %u channels\n", pid, capture.sample_rate(),
                capture.channels());

    IMMDeviceEnumerator* devices = nullptr;
    IMMDevice* endpoint = nullptr;
    IAudioSessionManager2* sessions = nullptr;
    IAudioEndpointVolume* endpoint_volume = nullptr;
    if (SUCCEEDED(CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                                   __uuidof(IMMDeviceEnumerator),
                                   reinterpret_cast<void**>(&devices))) &&
        SUCCEEDED(devices->GetDefaultAudioEndpoint(eRender, eConsole, &endpoint))) {
        endpoint->Activate(__uuidof(IAudioSessionManager2), CLSCTX_ALL, nullptr,
                           reinterpret_cast<void**>(&sessions));
        endpoint->Activate(__uuidof(IAudioEndpointVolume), CLSCTX_ALL, nullptr,
                           reinterpret_cast<void**>(&endpoint_volume));
    }

    FILE* wav = wav_path ? std::fopen(wav_path, "wb") : nullptr;
    uint64_t wav_bytes = 0;
    if (wav) write_wav_header(wav, 0);
    FILE* csv = csv_path ? std::fopen(csv_path, "w") : nullptr;
    if (csv) {
        std::fprintf(csv, "t_s,frames,rms_l_db,rms_r_db,peak_db,zero_pct,session_found,"
                          "mixer_volume,mixer_muted,session_peak_db,endpoint_volume\n");
    }

    std::mutex bucket_mutex;
    Bucket bucket{};
    std::mutex wav_mutex;
    const uint32_t channels = capture.channels();
    capture.start([&](const int16_t* samples, size_t count) {
        Bucket local{};
        for (size_t i = 0; i < count; ++i) {
            const int32_t v = samples[i];
            const double d = v / 32768.0;
            if (channels < 2 || i % 2 == 0) local.sum_sq_l += d * d;
            else local.sum_sq_r += d * d;
            local.peak = std::max(local.peak, v < 0 ? -v : v);
            if (v == 0) ++local.zero_samples;
        }
        local.samples = count;
        {
            std::lock_guard<std::mutex> lock(bucket_mutex);
            bucket.sum_sq_l += local.sum_sq_l;
            bucket.sum_sq_r += local.sum_sq_r;
            bucket.peak = std::max(bucket.peak, local.peak);
            bucket.zero_samples += local.zero_samples;
            bucket.samples += local.samples;
        }
        if (wav && channels == 2) {
            std::lock_guard<std::mutex> lock(wav_mutex);
            std::fwrite(samples, sizeof(int16_t), count, wav);
            wav_bytes += count * sizeof(int16_t);
        }
    });

    std::printf("t_s frames  rms_l  rms_r  peak  zero%%  mixer  mute  s_peak  endpoint\n");
    int silent = 0, digital_zero = 0, no_frames = 0;
    std::vector<double> levels;
    const auto start = std::chrono::steady_clock::now();
    for (int t = 1; t <= seconds; ++t) {
        std::this_thread::sleep_until(start + std::chrono::seconds(t));
        Bucket b{};
        {
            std::lock_guard<std::mutex> lock(bucket_mutex);
            b = bucket;
            bucket = Bucket{};
        }
        const uint64_t frames = channels ? b.samples / channels : 0;
        const double per_ch = frames ? static_cast<double>(frames) : 1.0;
        const double rms_l = to_db(std::sqrt(b.sum_sq_l / per_ch));
        const double rms_r = to_db(std::sqrt(b.sum_sq_r / per_ch));
        const double peak = to_db(b.peak / 32768.0);
        const double zero_pct = b.samples ? 100.0 * b.zero_samples / b.samples : 100.0;
        for (const auto& [at, value] : slider_steps) {
            if (at == t && !set_session_volume(sessions, process_tree(pid), value)) {
                std::printf("slider step at %d s: no session to set\n", t);
            }
        }
        const Session s = read_session(sessions, process_tree(pid));
        float ep_volume = -1.0f;
        if (endpoint_volume) endpoint_volume->GetMasterVolumeLevelScalar(&ep_volume);
        const double s_peak = s.peak >= 0.0f ? to_db(s.peak) : -120.0;

        if (frames == 0) ++no_frames;
        if (b.samples && b.zero_samples == b.samples) ++digital_zero;
        if (std::max(rms_l, rms_r) < -60.0) ++silent;
        levels.push_back(std::max(rms_l, rms_r));

        std::printf("%4d %6llu %6.1f %6.1f %5.1f %5.1f  %5.2f  %4d  %6.1f  %5.2f\n", t,
                    static_cast<unsigned long long>(frames), rms_l, rms_r, peak, zero_pct,
                    s.volume, s.muted ? 1 : 0, s_peak, ep_volume);
        std::fflush(stdout);
        if (csv) {
            std::fprintf(csv, "%d,%llu,%.2f,%.2f,%.2f,%.2f,%d,%.3f,%d,%.2f,%.3f\n", t,
                         static_cast<unsigned long long>(frames), rms_l, rms_r, peak, zero_pct,
                         s.found ? 1 : 0, s.volume, s.muted ? 1 : 0, s_peak, ep_volume);
            std::fflush(csv);
        }
    }

    capture.stop();
    if (wav) {
        std::lock_guard<std::mutex> lock(wav_mutex);
        write_wav_header(wav, static_cast<uint32_t>(std::min<uint64_t>(wav_bytes, 0xFFFFFFF0u)));
        std::fclose(wav);
    }
    if (csv) std::fclose(csv);

    std::sort(levels.begin(), levels.end());
    const double median = levels.empty() ? -120.0 : levels[levels.size() / 2];
    std::printf("summary: %d s, silent (< -60 dBFS) %d s, digital zero %d s, no frames %d s, "
                "median level %.1f dBFS\n",
                seconds, silent, digital_zero, no_frames, median);

    if (endpoint_volume) endpoint_volume->Release();
    if (sessions) sessions->Release();
    if (endpoint) endpoint->Release();
    if (devices) devices->Release();
    CoUninitialize();
    return 0;
}
