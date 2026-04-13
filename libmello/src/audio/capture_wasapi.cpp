#ifdef _WIN32
#include "capture_wasapi.hpp"
#include "../util/log.hpp"
#include <functiondiscoverykeys_devpkey.h>
#include <combaseapi.h>
#include <vector>
#include <cstring>
#include <algorithm>
#include <mmreg.h>
#include <ks.h>
#include <ksmedia.h>

namespace mello::audio {

static const CLSID CLSID_MMDeviceEnumerator_ = __uuidof(MMDeviceEnumerator);
static const IID IID_IMMDeviceEnumerator_ = __uuidof(IMMDeviceEnumerator);
static const IID IID_IAudioClient_ = __uuidof(IAudioClient);
static const IID IID_IAudioCaptureClient_ = __uuidof(IAudioCaptureClient);

static bool is_float_format(const WAVEFORMATEX* fmt) {
    if (!fmt) return false;
    if (fmt->wFormatTag == WAVE_FORMAT_IEEE_FLOAT) return true;
    if (fmt->wFormatTag == WAVE_FORMAT_EXTENSIBLE && fmt->cbSize >= 22) {
        auto* ext = reinterpret_cast<const WAVEFORMATEXTENSIBLE*>(fmt);
        return IsEqualGUID(ext->SubFormat, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    }
    return false;
}

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

    // Shared-mode capture uses the endpoint mix format; normalize to the
    // internal 48k mono contract in the capture thread.
    WAVEFORMATEX* mix_fmt = nullptr;
    hr = audio_client_->GetMixFormat(&mix_fmt);
    if (FAILED(hr)) return false;

    device_sample_rate_ = mix_fmt->nSamplesPerSec;
    device_channels_ = std::max<uint16_t>(1, mix_fmt->nChannels);
    device_float_format_ = is_float_format(mix_fmt);
    device_bits_per_sample_ = mix_fmt->wBitsPerSample;
    if (!device_float_format_ && device_bits_per_sample_ != 16) {
        MELLO_LOG_ERROR(
            "capture",
            "unsupported capture mix format: bits=%u tag=0x%04x",
            device_bits_per_sample_,
            mix_fmt->wFormatTag);
        CoTaskMemFree(mix_fmt);
        return false;
    }
    sample_rate_ = 48000;
    channels_ = 1;

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

    MELLO_LOG_INFO(
        "capture",
        "initialized: device_rate=%u device_ch=%u fmt=%s -> internal_rate=%u internal_ch=%u buf=%u frames",
        device_sample_rate_,
        device_channels_,
        device_float_format_ ? "f32" : "i16",
        sample_rate_,
        channels_,
        buffer_frames_);
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
    resample_src_pos_ = 0.0;
    src_mono_f32_.clear();
    resampled_i16_.clear();

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
                src_mono_f32_.assign(frames, 0.0f);
                if (!(flags & AUDCLNT_BUFFERFLAGS_SILENT)) {
                    if (device_float_format_) {
                        const float* fdata = reinterpret_cast<const float*>(data);
                        for (UINT32 i = 0; i < frames; ++i) {
                            float sum = 0.0f;
                            for (uint32_t ch = 0; ch < device_channels_; ++ch) {
                                sum += fdata[i * device_channels_ + ch];
                            }
                            src_mono_f32_[i] = sum / static_cast<float>(device_channels_);
                        }
                    } else {
                        const int16_t* idata = reinterpret_cast<const int16_t*>(data);
                        for (UINT32 i = 0; i < frames; ++i) {
                            float sum = 0.0f;
                            for (uint32_t ch = 0; ch < device_channels_; ++ch) {
                                sum += static_cast<float>(idata[i * device_channels_ + ch]) / 32768.0f;
                            }
                            src_mono_f32_[i] = sum / static_cast<float>(device_channels_);
                        }
                    }
                }

                resampled_i16_.clear();
                if (device_sample_rate_ == sample_rate_) {
                    resampled_i16_.resize(frames);
                    for (UINT32 i = 0; i < frames; ++i) {
                        float s = std::clamp(src_mono_f32_[i], -1.0f, 1.0f);
                        resampled_i16_[i] = static_cast<int16_t>(s * 32767.0f);
                    }
                } else {
                    const double step =
                        static_cast<double>(device_sample_rate_) / static_cast<double>(sample_rate_);
                    while (resample_src_pos_ < static_cast<double>(src_mono_f32_.size())) {
                        size_t idx = static_cast<size_t>(resample_src_pos_);
                        double frac = resample_src_pos_ - static_cast<double>(idx);
                        float s0 = src_mono_f32_[idx];
                        float s1 = (idx + 1 < src_mono_f32_.size()) ? src_mono_f32_[idx + 1] : s0;
                        float sample = std::clamp(s0 + static_cast<float>((s1 - s0) * frac), -1.0f, 1.0f);
                        resampled_i16_.push_back(static_cast<int16_t>(sample * 32767.0f));
                        resample_src_pos_ += step;
                    }
                    resample_src_pos_ -= static_cast<double>(src_mono_f32_.size());
                }

                if (!resampled_i16_.empty()) {
                    callback_(resampled_i16_.data(), resampled_i16_.size());
                }
            }

            capture_client_->ReleaseBuffer(frames);
        }
    }
    CoUninitialize();
}

} // namespace mello::audio
#endif
