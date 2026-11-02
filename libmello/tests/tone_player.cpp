// The real loopback row of the stream gate's replay lane (plans/GAME-CLIPS.md
// 10.3): a process that plays a 1 kHz tone in bursts, so the replay records
// it through WASAPI process loopback as it records a game. No ffmpeg or
// ffplay: they are GPL, and m3llo is Apache 2.0.
//
// Each burst's onset is written to a CSV as the steady clock (QPC, the clock
// of mello_clock_now_us) at which the device plays its first sample, from the
// audio clock's position. The gate compares them with the clip's frame codes.
//
//   mello_tone_player --seconds 40 --period-ms 1000 --on-ms 100 --csv onsets.csv
//
// Windows only. Not a test: nothing runs it in ctest.

#include <audioclient.h>
#include <mmdeviceapi.h>
#include <windows.h>

#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace {

struct Options {
    double seconds = 40.0;
    uint32_t period_ms = 1000;
    uint32_t on_ms = 100;
    double freq = 1000.0;
    std::string csv = "onsets.csv";
};

bool parse(int argc, char** argv, Options& o) {
    for (int i = 1; i + 1 < argc; i += 2) {
        const std::string k = argv[i];
        const char* v = argv[i + 1];
        if (k == "--seconds") o.seconds = std::atof(v);
        else if (k == "--period-ms") o.period_ms = static_cast<uint32_t>(std::atoi(v));
        else if (k == "--on-ms") o.on_ms = static_cast<uint32_t>(std::atoi(v));
        else if (k == "--freq") o.freq = std::atof(v);
        else if (k == "--csv") o.csv = v;
        else return false;
    }
    return o.seconds > 0 && o.period_ms > o.on_ms && o.on_ms > 0;
}

/// The steady clock in µs: std::chrono::steady_clock is QPC on Windows.
int64_t steady_us() {
    return std::chrono::duration_cast<std::chrono::microseconds>(
               std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

/// QPC ticks (100 ns units, as IAudioClock gives them) to the steady clock.
int64_t qpc100ns_to_steady_us(UINT64 qpc_100ns) {
    // Both are QPC: the offset between them is the epoch of steady_clock,
    // which counts from the same QPC origin.
    LARGE_INTEGER now_qpc{}, freq{};
    QueryPerformanceCounter(&now_qpc);
    QueryPerformanceFrequency(&freq);
    const double now_100ns = static_cast<double>(now_qpc.QuadPart) * 1e7 / static_cast<double>(freq.QuadPart);
    const int64_t now = steady_us();
    return now - static_cast<int64_t>((now_100ns - static_cast<double>(qpc_100ns)) / 10.0);
}

}  // namespace

int main(int argc, char** argv) {
    Options o;
    if (!parse(argc, argv, o)) {
        std::fprintf(stderr,
                     "usage: mello_tone_player --seconds 40 --period-ms 1000 --on-ms 100 [--freq 1000] "
                     "--csv onsets.csv\n");
        return 2;
    }
    if (FAILED(CoInitializeEx(nullptr, COINIT_MULTITHREADED))) return 1;

    IMMDeviceEnumerator* enumerator = nullptr;
    IMMDevice* device = nullptr;
    IAudioClient* client = nullptr;
    IAudioRenderClient* render = nullptr;
    IAudioClock* clock = nullptr;
    WAVEFORMATEX* mix = nullptr;
    auto fail = [&](const char* what, HRESULT hr) {
        std::fprintf(stderr, "%s failed: hr=0x%08lX\n", what, static_cast<unsigned long>(hr));
        return 1;
    };
    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                                  __uuidof(IMMDeviceEnumerator), reinterpret_cast<void**>(&enumerator));
    if (FAILED(hr)) return fail("MMDeviceEnumerator", hr);
    hr = enumerator->GetDefaultAudioEndpoint(eRender, eConsole, &device);
    if (FAILED(hr)) return fail("GetDefaultAudioEndpoint", hr);
    hr = device->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, reinterpret_cast<void**>(&client));
    if (FAILED(hr)) return fail("Activate", hr);
    hr = client->GetMixFormat(&mix);
    if (FAILED(hr)) return fail("GetMixFormat", hr);
    // The shared-mode mix format is float on every current Windows.
    const bool is_float =
        mix->wFormatTag == WAVE_FORMAT_IEEE_FLOAT ||
        (mix->wFormatTag == WAVE_FORMAT_EXTENSIBLE &&
         reinterpret_cast<WAVEFORMATEXTENSIBLE*>(mix)->SubFormat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    if (!is_float || mix->wBitsPerSample != 32) {
        std::fprintf(stderr, "the mix format is not 32-bit float\n");
        return 1;
    }
    const uint32_t rate = mix->nSamplesPerSec;
    const uint32_t channels = mix->nChannels;
    hr = client->Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 200'000 /* 20 ms */, 0, mix, nullptr);
    if (FAILED(hr)) return fail("Initialize", hr);
    UINT32 buffer_frames = 0;
    client->GetBufferSize(&buffer_frames);
    hr = client->GetService(__uuidof(IAudioRenderClient), reinterpret_cast<void**>(&render));
    if (FAILED(hr)) return fail("IAudioRenderClient", hr);
    hr = client->GetService(__uuidof(IAudioClock), reinterpret_cast<void**>(&clock));
    if (FAILED(hr)) return fail("IAudioClock", hr);
    UINT64 clock_freq = 0;
    clock->GetFrequency(&clock_freq);

    const uint64_t period = static_cast<uint64_t>(rate) * o.period_ms / 1000;
    const uint64_t on = static_cast<uint64_t>(rate) * o.on_ms / 1000;
    const uint64_t total = static_cast<uint64_t>(o.seconds * rate);
    const double step = 2.0 * 3.14159265358979323846 * o.freq / rate;

    FILE* csv = std::fopen(o.csv.c_str(), "w");
    if (!csv) {
        std::fprintf(stderr, "cannot write %s\n", o.csv.c_str());
        return 1;
    }
    std::fprintf(csv, "burst,onset_frame,onset_steady_us\n");
    std::printf("pid %lu, %u Hz, %u channels, a %u ms burst every %u ms for %.1f s\n",
                static_cast<unsigned long>(GetCurrentProcessId()), rate, channels, o.on_ms, o.period_ms,
                o.seconds);
    std::fflush(stdout);

    uint64_t written = 0;
    uint64_t next_burst = period;  // the first burst after a second of silence
    uint64_t logged = 0;
    client->Start();
    while (written < total) {
        UINT32 padding = 0;
        client->GetCurrentPadding(&padding);
        const UINT32 room = buffer_frames - padding;
        if (room > 0) {
            BYTE* data = nullptr;
            if (SUCCEEDED(render->GetBuffer(room, &data))) {
                auto* out = reinterpret_cast<float*>(data);
                for (UINT32 i = 0; i < room; ++i) {
                    const uint64_t f = written + i;
                    const uint64_t in_period = f % period;
                    const bool sounding = f >= period && in_period < on;
                    const float v = sounding ? static_cast<float>(0.3 * std::sin(step * static_cast<double>(in_period))) : 0.0f;
                    for (uint32_t c = 0; c < channels; ++c) out[i * channels + c] = v;
                }
                render->ReleaseBuffer(room, 0);
                written += room;
            }
        }
        // The onset of each burst the device has played: the clock's
        // position with its QPC time, back to the burst's first frame.
        UINT64 pos = 0, qpc = 0;
        if (SUCCEEDED(clock->GetPosition(&pos, &qpc)) && clock_freq > 0) {
            const uint64_t played = pos * rate / clock_freq;
            while (next_burst <= played) {
                const int64_t at_pos = qpc100ns_to_steady_us(qpc);
                const int64_t onset = at_pos - static_cast<int64_t>((played - next_burst) * 1'000'000 / rate);
                std::fprintf(csv, "%llu,%llu,%lld\n", static_cast<unsigned long long>(logged),
                             static_cast<unsigned long long>(next_burst), static_cast<long long>(onset));
                ++logged;
                next_burst += period;
            }
        }
        Sleep(5);
    }
    Sleep(200);
    client->Stop();
    std::fclose(csv);
    std::printf("%llu bursts\n", static_cast<unsigned long long>(logged));
    clock->Release();
    render->Release();
    client->Release();
    CoTaskMemFree(mix);
    device->Release();
    enumerator->Release();
    CoUninitialize();
    return 0;
}
