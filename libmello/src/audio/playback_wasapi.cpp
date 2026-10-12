#ifdef _WIN32
#include "playback_wasapi.hpp"
#include "audio_session_win.hpp"
#include "../util/log.hpp"
#include <combaseapi.h>
#include <vector>
#include <algorithm>
#include <cmath>
#include <cstring>
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

    cached_output_latency_ms_ = query_output_latency_ms();

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
    resampler_.reset();
    if (device_sample_rate_ != sample_rate_) {
        resampler_ = std::make_unique<StreamResampler>(
            static_cast<int>(sample_rate_), static_cast<int>(device_sample_rate_),
            static_cast<int>(input_channels_));
        MELLO_LOG_INFO("playback", "sinc resampler %u -> %u Hz, %u ch, blocks %zu -> %zu frames",
                       sample_rate_, device_sample_rate_, input_channels_,
                       resampler_->in_block_frames(), resampler_->out_block_frames());
    }

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

        const size_t in_ch = static_cast<size_t>(input_channels_);
        const size_t samples_needed = static_cast<size_t>(available) * in_ch;
        mono_buf.assign(samples_needed, 0);

        // The render source (voice mix) or the feed() ring (stream audio).
        auto pull_source = [this](int16_t* dst, size_t count) -> size_t {
            const size_t got = render_source_ ? render_source_(dst, count) : ring_.read(dst, count);
            if (got < count) std::memset(dst + got, 0, (count - got) * sizeof(int16_t));
            return got;
        };

        if (!resampler_) {
            const size_t got = pull_source(mono_buf.data(), samples_needed);
            if (pb_log_ctr < 20 || (pb_log_ctr % 2000) == 0) {
                MELLO_LOG_DEBUG(
                    "playback",
                    "render: got=%zu avail=%u device_rate=%u ch=%u in_ch=%u src=%s",
                    got,
                    available,
                    device_sample_rate_,
                    device_channels_,
                    input_channels_,
                    render_source_ ? "mix" : "ring");
            }
        } else {
            // Feed the sinc resampler whole internal blocks (10 ms at 48 kHz,
            // the mixer's frame) until it holds this callback's frames. The
            // rest stays in the resampler for the next callback.
            const size_t block = resampler_->in_block_frames();
            while (resampler_->available() < available) {
                render_src_i16_.resize(block * in_ch);
                pull_source(render_src_i16_.data(), block * in_ch);
                render_src_f32_.resize(block * in_ch);
                for (size_t i = 0; i < block * in_ch; ++i) {
                    render_src_f32_[i] = static_cast<float>(render_src_i16_[i]) / 32768.0f;
                }
                resampler_->push(render_src_f32_.data(), block);
            }
            render_dst_f32_.resize(samples_needed);
            resampler_->pull(render_dst_f32_.data(), available);
            for (size_t i = 0; i < samples_needed; ++i) {
                const float v = std::clamp(render_dst_f32_[i] * 32768.0f, -32768.0f, 32767.0f);
                mono_buf[i] = static_cast<int16_t>(std::lrintf(v));
            }

            if (pb_log_ctr < 20 || (pb_log_ctr % 2000) == 0) {
                MELLO_LOG_DEBUG(
                    "playback",
                    "render: avail=%u internal_rate=%u device_rate=%u ch=%u buffered=%zu",
                    available,
                    sample_rate_,
                    device_sample_rate_,
                    input_channels_,
                    resampler_->available());
            }
        }
        pb_log_ctr++;

        // Channel map: mono goes to every device channel. Stereo goes to the
        // first two (front left and right); a surround device gets silence
        // on the rest, the standard stereo-on-surround placement. A mono
        // device gets the downmix.
        auto device_sample = [&](UINT32 frame, uint32_t dev_ch) -> int16_t {
            if (input_channels_ == 1) return mono_buf[frame];
            const size_t base = static_cast<size_t>(frame) * input_channels_;
            if (device_channels_ == 1) {
                // A mono device plays the downmix of the first two channels.
                return static_cast<int16_t>((static_cast<int32_t>(mono_buf[base]) +
                                             static_cast<int32_t>(mono_buf[base + 1])) / 2);
            }
            if (dev_ch >= input_channels_) return 0;
            return mono_buf[static_cast<size_t>(frame) * input_channels_ + dev_ch];
        };
        if (device_float_format_) {
            float* fdata = reinterpret_cast<float*>(data);
            for (UINT32 i = 0; i < available; ++i) {
                for (uint32_t dev_ch = 0; dev_ch < device_channels_; ++dev_ch) {
                    fdata[i * device_channels_ + dev_ch] =
                        static_cast<float>(device_sample(i, dev_ch)) / 32768.0f;
                }
            }
        } else {
            int16_t* idata = reinterpret_cast<int16_t*>(data);
            for (UINT32 i = 0; i < available; ++i) {
                for (uint32_t dev_ch = 0; dev_ch < device_channels_; ++dev_ch) {
                    idata[i * device_channels_ + dev_ch] = device_sample(i, dev_ch);
                }
            }
        }

        render_client_->ReleaseBuffer(available, 0);
    }
    CoUninitialize();
}

int WasapiPlayback::query_output_latency_ms() {
    if (!audio_client_) return 0;
    double total_ms = 0.0;

    REFERENCE_TIME stream_latency = 0;
    HRESULT hr = audio_client_->GetStreamLatency(&stream_latency);
    if (SUCCEEDED(hr) && stream_latency > 0) {
        total_ms += static_cast<double>(stream_latency) / 10000.0;
    } else if (FAILED(hr)) {
        MELLO_LOG_INFO("playback", "GetStreamLatency failed hr=0x%08lx, using 0", hr);
    } else {
        MELLO_LOG_DEBUG("playback", "GetStreamLatency returned 0 (shared mode), using 0");
    }

    REFERENCE_TIME default_period = 0, min_period = 0;
    hr = audio_client_->GetDevicePeriod(&default_period, &min_period);
    if (SUCCEEDED(hr) && default_period > 0) {
        total_ms += static_cast<double>(default_period) / 10000.0;
    } else if (FAILED(hr)) {
        MELLO_LOG_INFO("playback", "GetDevicePeriod failed hr=0x%08lx, using 0", hr);
    } else {
        MELLO_LOG_DEBUG("playback", "GetDevicePeriod returned 0, using 0");
    }

    if (total_ms < 0) total_ms = 0;
    if (total_ms > 500) total_ms = 500;
    int ms = static_cast<int>(total_ms + 0.5);
    MELLO_LOG_INFO("playback", "WASAPI output latency estimate %d ms", ms);
    return ms;
}

} // namespace mello::audio
#endif
