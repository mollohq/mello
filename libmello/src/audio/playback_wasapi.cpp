#ifdef _WIN32
#include "playback_wasapi.hpp"
#include <combaseapi.h>
#include <vector>

namespace mello::audio {

static const CLSID CLSID_MMDeviceEnumerator_PB = __uuidof(MMDeviceEnumerator);
static const IID IID_IMMDeviceEnumerator_PB = __uuidof(IMMDeviceEnumerator);
static const IID IID_IAudioClient_PB = __uuidof(IAudioClient);
static const IID IID_IAudioRenderClient_PB = __uuidof(IAudioRenderClient);

WasapiPlayback::WasapiPlayback() = default;

WasapiPlayback::~WasapiPlayback() {
    stop();
    if (event_) CloseHandle(event_);
    if (render_client_) render_client_->Release();
    if (audio_client_) audio_client_->Release();
    if (device_) device_->Release();
}

bool WasapiPlayback::initialize(const char* /*device_id*/) {
    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(hr) && hr != S_FALSE && hr != RPC_E_CHANGED_MODE) return false;

    IMMDeviceEnumerator* enumerator = nullptr;
    hr = CoCreateInstance(
        CLSID_MMDeviceEnumerator_PB, nullptr, CLSCTX_ALL,
        IID_IMMDeviceEnumerator_PB, reinterpret_cast<void**>(&enumerator));
    if (FAILED(hr)) return false;

    hr = enumerator->GetDefaultAudioEndpoint(eRender, eCommunications, &device_);
    enumerator->Release();
    if (FAILED(hr)) return false;

    hr = device_->Activate(IID_IAudioClient_PB, CLSCTX_ALL, nullptr,
                           reinterpret_cast<void**>(&audio_client_));
    if (FAILED(hr)) return false;

    WAVEFORMATEX* mix_fmt = nullptr;
    hr = audio_client_->GetMixFormat(&mix_fmt);
    if (FAILED(hr)) return false;

    sample_rate_ = mix_fmt->nSamplesPerSec;
    device_channels_ = mix_fmt->nChannels;

    event_ = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!event_) { CoTaskMemFree(mix_fmt); return false; }

    REFERENCE_TIME duration = 200000; // 20ms
    hr = audio_client_->Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        duration, 0, mix_fmt, nullptr);
    CoTaskMemFree(mix_fmt);
    if (FAILED(hr)) return false;

    hr = audio_client_->SetEventHandle(event_);
    if (FAILED(hr)) return false;

    hr = audio_client_->GetBufferSize(&buffer_frames_);
    if (FAILED(hr)) return false;

    hr = audio_client_->GetService(IID_IAudioRenderClient_PB,
                                   reinterpret_cast<void**>(&render_client_));
    if (FAILED(hr)) return false;

    return true;
}

bool WasapiPlayback::start() {
    if (running_) return false;
    running_ = true;

    HRESULT hr = audio_client_->Start();
    if (FAILED(hr)) { running_ = false; return false; }

    thread_ = std::thread(&WasapiPlayback::playback_thread, this);
    return true;
}

void WasapiPlayback::stop() {
    if (!running_) return;
    running_ = false;
    if (event_) SetEvent(event_);
    if (thread_.joinable()) thread_.join();
    if (audio_client_) audio_client_->Stop();
}

size_t WasapiPlayback::feed(const int16_t* samples, size_t count) {
    return ring_.write(samples, count);
}

void WasapiPlayback::playback_thread() {
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    std::vector<int16_t> mono_buf;

    while (running_) {
        DWORD wait = WaitForSingleObject(event_, 100);
        if (!running_) break;
        if (wait != WAIT_OBJECT_0) continue;

        UINT32 padding = 0;
        audio_client_->GetCurrentPadding(&padding);
        UINT32 available = buffer_frames_ - padding;
        if (available == 0) continue;

        BYTE* data = nullptr;
        HRESULT hr = render_client_->GetBuffer(available, &data);
        if (FAILED(hr)) continue;

        // Read mono samples from ring buffer
        mono_buf.resize(available);
        size_t got = ring_.read(mono_buf.data(), available);

        // Zero-fill if we don't have enough samples (underrun)
        if (got < available) {
            std::memset(&mono_buf[got], 0, (available - got) * sizeof(int16_t));
        }

        // Convert mono int16 -> device float32 multi-channel
        float* fdata = reinterpret_cast<float*>(data);
        for (UINT32 i = 0; i < available; ++i) {
            float sample = static_cast<float>(mono_buf[i]) / 32768.0f;
            for (uint32_t ch = 0; ch < device_channels_; ++ch) {
                fdata[i * device_channels_ + ch] = sample;
            }
        }

        render_client_->ReleaseBuffer(available, 0);
    }
    CoUninitialize();
}

} // namespace mello::audio
#endif
