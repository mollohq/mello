#ifdef _WIN32
#include "playback_wasapi.hpp"
#include "audio_session_win.hpp"
#include "../util/log.hpp"
#include <combaseapi.h>
#include <vector>
#include <algorithm>
#include <cmath>
#include <mmreg.h>
#include <ks.h>
#include <ksmedia.h>

namespace mello::audio {

static const CLSID CLSID_MMDeviceEnumerator_PB = __uuidof(MMDeviceEnumerator);
static const IID IID_IMMDeviceEnumerator_PB = __uuidof(IMMDeviceEnumerator);
static const IID IID_IAudioClient_PB = __uuidof(IAudioClient);
static const IID IID_IAudioRenderClient_PB = __uuidof(IAudioRenderClient);

static bool is_float_format(const WAVEFORMATEX* fmt) {
    if (!fmt) return false;
    if (fmt->wFormatTag == WAVE_FORMAT_IEEE_FLOAT) return true;
    if (fmt->wFormatTag == WAVE_FORMAT_EXTENSIBLE && fmt->cbSize >= 22) {
        auto* ext = reinterpret_cast<const WAVEFORMATEXTENSIBLE*>(fmt);
        return IsEqualGUID(ext->SubFormat, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    }
    return false;
}

WasapiPlayback::WasapiPlayback() = default;

WasapiPlayback::~WasapiPlayback() {
    stop();
    if (event_) CloseHandle(event_);
    if (render_client_) render_client_->Release();
    if (audio_client_) audio_client_->Release();
    if (device_) device_->Release();
}

bool WasapiPlayback::initialize(const char* device_id) {
    MELLO_LOG_INFO("playback", "initializing (device=%s)", device_id ? device_id : "default");

    HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    if (FAILED(hr) && hr != S_FALSE && hr != RPC_E_CHANGED_MODE) {
        MELLO_LOG_ERROR("playback", "COM init failed hr=0x%08lx", hr);
        return false;
    }

    IMMDeviceEnumerator* enumerator = nullptr;
    hr = CoCreateInstance(
        CLSID_MMDeviceEnumerator_PB, nullptr, CLSCTX_ALL,
        IID_IMMDeviceEnumerator_PB, reinterpret_cast<void**>(&enumerator));
    if (FAILED(hr)) return false;

    if (device_id && device_id[0] != '\0') {
        int len = MultiByteToWideChar(CP_UTF8, 0, device_id, -1, nullptr, 0);
        std::vector<wchar_t> wid(len);
        MultiByteToWideChar(CP_UTF8, 0, device_id, -1, wid.data(), len);
        hr = enumerator->GetDevice(wid.data(), &device_);
    } else {
        hr = enumerator->GetDefaultAudioEndpoint(eRender, eConsole, &device_);
    }
    enumerator->Release();
    if (FAILED(hr)) return false;

    hr = device_->Activate(IID_IAudioClient_PB, CLSCTX_ALL, nullptr,
                           reinterpret_cast<void**>(&audio_client_));
    if (FAILED(hr)) return false;

    WAVEFORMATEX* mix_fmt = nullptr;
    hr = audio_client_->GetMixFormat(&mix_fmt);
    if (FAILED(hr)) return false;

    device_sample_rate_ = mix_fmt->nSamplesPerSec;
    device_channels_ = std::max<uint16_t>(1, mix_fmt->nChannels);
    device_float_format_ = is_float_format(mix_fmt);
    device_bits_per_sample_ = mix_fmt->wBitsPerSample;
    if (!device_float_format_ && device_bits_per_sample_ != 16) {
        MELLO_LOG_ERROR(
            "playback",
            "unsupported playback mix format: bits=%u tag=0x%04x",
            device_bits_per_sample_,
            mix_fmt->wFormatTag);
        CoTaskMemFree(mix_fmt);
        return false;
    }
    sample_rate_ = 48000;

    event_ = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!event_) { CoTaskMemFree(mix_fmt); return false; }

    REFERENCE_TIME duration = 200000; // 20ms
    hr = audio_client_->Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        duration, 0, mix_fmt, nullptr);
    CoTaskMemFree(mix_fmt);
    if (FAILED(hr)) return false;

    if (session_win_) {
        session_win_->disable_ducking_for_client(audio_client_);
    }

    hr = audio_client_->SetEventHandle(event_);
    if (FAILED(hr)) return false;

    hr = audio_client_->GetBufferSize(&buffer_frames_);
    if (FAILED(hr)) return false;

    hr = audio_client_->GetService(IID_IAudioRenderClient_PB,
                                   reinterpret_cast<void**>(&render_client_));
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("playback", "GetService(RenderClient) failed hr=0x%08lx", hr);
        return false;
    }

    WAVEFORMATEX* log_fmt = nullptr;
    if (SUCCEEDED(audio_client_->GetMixFormat(&log_fmt))) {
        MELLO_LOG_INFO("playback", "mix_fmt: rate=%u ch=%u bits=%u tag=0x%04x",
                       log_fmt->nSamplesPerSec, log_fmt->nChannels,
                       log_fmt->wBitsPerSample, log_fmt->wFormatTag);
        CoTaskMemFree(log_fmt);
    }

    MELLO_LOG_INFO(
        "playback",
        "initialized: internal_rate=%u -> device_rate=%u ch=%u fmt=%s buf=%u frames",
        sample_rate_,
        device_sample_rate_,
        device_channels_,
        device_float_format_ ? "f32" : "i16",
        buffer_frames_);
    return true;
}

bool WasapiPlayback::start() {
    if (running_) return false;
    if (!audio_client_) {
        MELLO_LOG_ERROR("playback", "start() called but device not initialized");
        return false;
    }
    running_ = true;

    HRESULT hr = audio_client_->Start();
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("playback", "Start() failed hr=0x%08lx", hr);
        running_ = false;
        return false;
    }
    src_fifo_.clear();
    src_fifo_pos_ = 0.0;
    render_src_i16_.clear();

    thread_ = std::thread(&WasapiPlayback::playback_thread, this);
    MELLO_LOG_INFO("playback", "started");
    return true;
}

void WasapiPlayback::stop() {
    if (!running_) return;
    running_ = false;
    if (event_) SetEvent(event_);
    if (thread_.joinable()) thread_.join();
    if (audio_client_) audio_client_->Stop();
    MELLO_LOG_INFO("playback", "stopped");
}

size_t WasapiPlayback::feed(const int16_t* samples, size_t count) {
    return ring_.write(samples, count);
}

void WasapiPlayback::playback_thread() {
    CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    std::vector<int16_t> mono_buf;
    uint32_t pb_log_ctr = 0;

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

        mono_buf.assign(available, 0);

        if (device_sample_rate_ == sample_rate_) {
            size_t got = 0;
            if (render_source_) {
                got = render_source_(mono_buf.data(), available);
            } else {
                got = ring_.read(mono_buf.data(), available);
            }
            if (got < available) {
                std::memset(&mono_buf[got], 0, (available - got) * sizeof(int16_t));
            }
            if (pb_log_ctr < 20 || (pb_log_ctr % 2000) == 0) {
                MELLO_LOG_DEBUG(
                    "playback",
                    "render: got=%zu avail=%u device_rate=%u ch=%u src=%s",
                    got,
                    available,
                    device_sample_rate_,
                    device_channels_,
                    render_source_ ? "mix" : "ring");
            }
        } else {
            const double step =
                static_cast<double>(sample_rate_) / static_cast<double>(device_sample_rate_);
            const size_t min_src_needed =
                static_cast<size_t>(std::ceil(src_fifo_pos_ + (available + 1) * step)) + 2;

            while (src_fifo_.size() < min_src_needed) {
                const size_t request =
                    std::max<size_t>(480, static_cast<size_t>(std::ceil(available * step)) + 8);
                render_src_i16_.assign(request, 0);
                size_t got = 0;
                if (render_source_) {
                    got = render_source_(render_src_i16_.data(), request);
                } else {
                    got = ring_.read(render_src_i16_.data(), request);
                }
                if (got < request) {
                    std::memset(
                        &render_src_i16_[got], 0, (request - got) * sizeof(int16_t));
                }
                for (size_t i = 0; i < request; ++i) {
                    src_fifo_.push_back(static_cast<float>(render_src_i16_[i]) / 32768.0f);
                }
            }

            for (UINT32 i = 0; i < available; ++i) {
                double src_pos = src_fifo_pos_ + static_cast<double>(i) * step;
                size_t idx = static_cast<size_t>(src_pos);
                double frac = src_pos - static_cast<double>(idx);
                float s0 = src_fifo_[idx];
                float s1 = (idx + 1 < src_fifo_.size()) ? src_fifo_[idx + 1] : s0;
                float sample = std::clamp(s0 + static_cast<float>((s1 - s0) * frac), -1.0f, 1.0f);
                mono_buf[i] = static_cast<int16_t>(sample * 32767.0f);
            }

            src_fifo_pos_ += static_cast<double>(available) * step;
            size_t consumed = static_cast<size_t>(src_fifo_pos_);
            if (consumed > 0) {
                if (consumed >= src_fifo_.size()) {
                    src_fifo_.clear();
                } else {
                    src_fifo_.erase(src_fifo_.begin(), src_fifo_.begin() + consumed);
                }
                src_fifo_pos_ -= static_cast<double>(consumed);
            }

            if (pb_log_ctr < 20 || (pb_log_ctr % 2000) == 0) {
                MELLO_LOG_DEBUG(
                    "playback",
                    "render: avail=%u internal_rate=%u device_rate=%u step=%.4f fifo=%zu",
                    available,
                    sample_rate_,
                    device_sample_rate_,
                    step,
                    src_fifo_.size());
            }
        }
        pb_log_ctr++;

        if (device_float_format_) {
            float* fdata = reinterpret_cast<float*>(data);
            for (UINT32 i = 0; i < available; ++i) {
                float sample = static_cast<float>(mono_buf[i]) / 32768.0f;
                for (uint32_t ch = 0; ch < device_channels_; ++ch) {
                    fdata[i * device_channels_ + ch] = sample;
                }
            }
        } else {
            int16_t* idata = reinterpret_cast<int16_t*>(data);
            for (UINT32 i = 0; i < available; ++i) {
                int16_t sample = mono_buf[i];
                for (uint32_t ch = 0; ch < device_channels_; ++ch) {
                    idata[i * device_channels_ + ch] = sample;
                }
            }
        }

        render_client_->ReleaseBuffer(available, 0);
    }
    CoUninitialize();
}

} // namespace mello::audio
#endif
