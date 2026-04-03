#ifdef _WIN32
#include "capture_wasapi.hpp"
#include "../util/log.hpp"
#include <functiondiscoverykeys_devpkey.h>
#include <combaseapi.h>
#include <vector>
#include <cstring>

namespace mello::audio {

static const CLSID CLSID_MMDeviceEnumerator_ = __uuidof(MMDeviceEnumerator);
static const IID IID_IMMDeviceEnumerator_ = __uuidof(IMMDeviceEnumerator);
static const IID IID_IAudioClient_ = __uuidof(IAudioClient);
static const IID IID_IAudioCaptureClient_ = __uuidof(IAudioCaptureClient);

WasapiCapture::WasapiCapture() = default;

WasapiCapture::~WasapiCapture() {
    stop();
    if (event_) CloseHandle(event_);
    if (capture_client_) capture_client_->Release();
    if (audio_client_) audio_client_->Release();
    if (device_) device_->Release();
}

bool WasapiCapture::init_com() {
    HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    if (SUCCEEDED(hr) || hr == S_FALSE) {
        com_initialized_ = true;
        return true;
    }
    if (hr == RPC_E_CHANGED_MODE) {
        return true;
    }
    return false;
}

bool WasapiCapture::initialize(const char* device_id) {
    MELLO_LOG_INFO("capture", "initializing (device=%s)", device_id ? device_id : "default");

    if (!init_com()) {
        MELLO_LOG_ERROR("capture", "COM init failed");
        return false;
    }

    IMMDeviceEnumerator* enumerator = nullptr;
    HRESULT hr = CoCreateInstance(
        CLSID_MMDeviceEnumerator_, nullptr, CLSCTX_ALL,
        IID_IMMDeviceEnumerator_, reinterpret_cast<void**>(&enumerator));
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("capture", "CoCreateInstance failed hr=0x%08lx", hr);
        return false;
    }

    if (device_id && device_id[0] != '\0') {
        int len = MultiByteToWideChar(CP_UTF8, 0, device_id, -1, nullptr, 0);
        std::vector<wchar_t> wid(len);
        MultiByteToWideChar(CP_UTF8, 0, device_id, -1, wid.data(), len);
        hr = enumerator->GetDevice(wid.data(), &device_);
    } else {
        hr = enumerator->GetDefaultAudioEndpoint(eCapture, eConsole, &device_);
    }
    enumerator->Release();
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("capture", "device open failed hr=0x%08lx", hr);
        return false;
    }

    hr = device_->Activate(IID_IAudioClient_, CLSCTX_ALL, nullptr,
                           reinterpret_cast<void**>(&audio_client_));
    if (FAILED(hr)) return false;

    // Desired format: 48kHz mono 16-bit PCM
    WAVEFORMATEX desired = {};
    desired.wFormatTag = WAVE_FORMAT_PCM;
    desired.nChannels = 1;
    desired.nSamplesPerSec = 48000;
    desired.wBitsPerSample = 16;
    desired.nBlockAlign = desired.nChannels * desired.wBitsPerSample / 8;
    desired.nAvgBytesPerSec = desired.nSamplesPerSec * desired.nBlockAlign;

    // Try our desired format first; fall back to mix format
    WAVEFORMATEX* closest = nullptr;
    hr = audio_client_->IsFormatSupported(AUDCLNT_SHAREMODE_SHARED, &desired, &closest);
    if (hr == S_OK) {
        // Desired format is supported directly
        sample_rate_ = desired.nSamplesPerSec;
        channels_ = desired.nChannels;
        if (closest) CoTaskMemFree(closest);
    } else {
        // Use the device's mix format and we'll resample/convert in the capture thread
        if (closest) CoTaskMemFree(closest);
        WAVEFORMATEX* mix = nullptr;
        hr = audio_client_->GetMixFormat(&mix);
        if (FAILED(hr)) return false;
        sample_rate_ = mix->nSamplesPerSec;
        channels_ = mix->nChannels;
        CoTaskMemFree(mix);
    }

    // Re-get mix format for initialization (shared mode requires device format)
    WAVEFORMATEX* mix_fmt = nullptr;
    hr = audio_client_->GetMixFormat(&mix_fmt);
    if (FAILED(hr)) return false;

    sample_rate_ = mix_fmt->nSamplesPerSec;
    channels_ = mix_fmt->nChannels;

    event_ = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!event_) { CoTaskMemFree(mix_fmt); return false; }

    // 20ms buffer
    REFERENCE_TIME duration = 200000; // 20ms in 100ns units
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

    hr = audio_client_->GetService(IID_IAudioCaptureClient_,
                                   reinterpret_cast<void**>(&capture_client_));
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("capture", "GetService(CaptureClient) failed hr=0x%08lx", hr);
        return false;
    }

    MELLO_LOG_INFO("capture", "initialized: rate=%u ch=%u buf=%u frames",
                   sample_rate_, channels_, buffer_frames_);
    return true;
}

bool WasapiCapture::start(Callback callback) {
    if (running_) return false;
    callback_ = std::move(callback);
    running_ = true;

    HRESULT hr = audio_client_->Start();
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("capture", "Start() failed hr=0x%08lx", hr);
        running_ = false;
        return false;
    }

    thread_ = std::thread(&WasapiCapture::capture_thread, this);
    MELLO_LOG_INFO("capture", "started");
    return true;
}

void WasapiCapture::stop() {
    if (!running_) return;
    running_ = false;
    if (event_) SetEvent(event_);
    if (thread_.joinable()) thread_.join();
    if (audio_client_) audio_client_->Stop();
    MELLO_LOG_INFO("capture", "stopped");
}

void WasapiCapture::capture_thread() {
    CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    // Resampling buffer: we convert from device format to mono 16-bit
    std::vector<int16_t> mono_buf;

    while (running_) {
        DWORD wait = WaitForSingleObject(event_, 100);
        if (!running_) break;
        if (wait != WAIT_OBJECT_0) continue;

        UINT32 packet_len = 0;
        while (SUCCEEDED(capture_client_->GetNextPacketSize(&packet_len)) && packet_len > 0) {
            BYTE* data = nullptr;
            UINT32 frames = 0;
            DWORD flags = 0;
            HRESULT hr = capture_client_->GetBuffer(&data, &frames, &flags, nullptr, nullptr);
            if (FAILED(hr)) break;

            if (callback_ && frames > 0) {
                if (flags & AUDCLNT_BUFFERFLAGS_SILENT) {
                    // Send silence
                    mono_buf.assign(frames, 0);
                    callback_(mono_buf.data(), frames);
                } else {
                    // Device gave us float32 samples (typical WASAPI shared mode)
                    // Downmix to mono int16
                    const float* fdata = reinterpret_cast<const float*>(data);
                    mono_buf.resize(frames);
                    for (UINT32 i = 0; i < frames; ++i) {
                        float sum = 0.0f;
                        for (uint32_t ch = 0; ch < channels_; ++ch) {
                            sum += fdata[i * channels_ + ch];
                        }
                        float mono = sum / static_cast<float>(channels_);
                        // Clamp and convert to int16
                        mono = (mono < -1.0f) ? -1.0f : (mono > 1.0f) ? 1.0f : mono;
                        mono_buf[i] = static_cast<int16_t>(mono * 32767.0f);
                    }
                    callback_(mono_buf.data(), frames);
                }
            }

            capture_client_->ReleaseBuffer(frames);
        }
    }
    CoUninitialize();
}

} // namespace mello::audio
#endif
