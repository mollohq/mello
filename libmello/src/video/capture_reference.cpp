#include "capture_reference.hpp"
#include "../util/log.hpp"

#include <algorithm>
#include <chrono>
#include <cstring>
#include <mutex>
#include <thread>
#include <vector>

#ifdef _WIN32
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <d3d11.h>
#include <wrl/client.h>
using Microsoft::WRL::ComPtr;
#ifndef CREATE_WAITABLE_TIMER_HIGH_RESOLUTION
#define CREATE_WAITABLE_TIMER_HIGH_RESOLUTION 0x00000002
#endif
#endif

namespace mello::video {

static constexpr const char* TAG = "video/reference";

uint64_t reference_clip_frames(uint64_t file_bytes, uint32_t width, uint32_t height) {
    if (width == 0 || height == 0 || (width & 1u) || (height & 1u)) return 0;
    const uint64_t frame = reference_frame_bytes(width, height);
    if (file_bytes == 0 || file_bytes % frame != 0) return 0;
    return file_bytes / frame;
}

void reference_i420_to_nv12(const uint8_t* i420, uint32_t width, uint32_t height, uint8_t* nv12) {
    const size_t luma = static_cast<size_t>(width) * height;
    std::memcpy(nv12, i420, luma);
    const uint8_t* u = i420 + luma;
    const uint8_t* v = u + luma / 4;
    uint8_t* uv = nv12 + luma;
    const size_t chroma = luma / 4;
    for (size_t i = 0; i < chroma; ++i) {
        uv[i * 2] = u[i];
        uv[i * 2 + 1] = v[i];
    }
}

#ifdef _WIN32

static uint64_t steady_now_us() {
    // The clock of video_pipeline.cpp now_us() and of mello_clock_now_us().
    return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::microseconds>(
                                     std::chrono::steady_clock::now().time_since_epoch())
                                     .count());
}

static std::wstring utf8_to_wide(const std::string& s) {
    if (s.empty()) return {};
    const int n = MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), nullptr, 0);
    std::wstring w(static_cast<size_t>(n), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), w.data(), n);
    return w;
}

struct ReferenceCapture::Impl {
    ReferenceClip clip;
    uint64_t      frames = 0;
    uint64_t      frame_bytes = 0;
    HANDLE        file = INVALID_HANDLE_VALUE;

    ComPtr<ID3D11Device>                     device;
    ComPtr<ID3D11DeviceContext>              context;
    ComPtr<ID3D11VideoDevice>                video_device;
    ComPtr<ID3D11VideoContext>               video_context;
    ComPtr<ID3D11VideoProcessorEnumerator>   vp_enum;
    ComPtr<ID3D11VideoProcessor>             vp;
    ComPtr<ID3D11Texture2D>                  nv12_tex;
    ComPtr<ID3D11VideoProcessorInputView>    input_view;
    static constexpr size_t kSlots = 2;
    ComPtr<ID3D11Texture2D>                  bgra[kSlots];
    ComPtr<ID3D11VideoProcessorOutputView>   output_view[kSlots];

    std::vector<uint8_t> raw;   // one file frame
    std::vector<uint8_t> nv12;  // the same frame packed as NV12

    std::thread       thread;
    std::atomic<bool> running{false};
    HANDLE            timer = nullptr;

    // Stage 7: the clip's audio, float stereo, one clip loop long.
    std::vector<float> audio;
    std::thread        audio_thread;
    HANDLE             audio_timer = nullptr;
    std::mutex         audio_cb_mutex;
    CaptureSource::TimedAudioCallback audio_cb;

    mutable std::mutex info_mutex;
    ReferenceInfo      info{};

    ~Impl() {
        if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
        if (timer) CloseHandle(timer);
        if (audio_timer) CloseHandle(audio_timer);
    }

    bool read_frame(uint64_t index) {
        LARGE_INTEGER offset{};
        offset.QuadPart = static_cast<LONGLONG>(index * frame_bytes);
        if (!SetFilePointerEx(file, offset, nullptr, FILE_BEGIN)) return false;
        size_t done = 0;
        while (done < frame_bytes) {
            const DWORD want = static_cast<DWORD>(std::min<uint64_t>(frame_bytes - done, 1u << 30));
            DWORD got = 0;
            if (!ReadFile(file, raw.data() + done, want, &got, nullptr) || got == 0) return false;
            done += got;
        }
        if (clip.nv12) {
            std::memcpy(nv12.data(), raw.data(), raw.size());
        } else {
            reference_i420_to_nv12(raw.data(), clip.width, clip.height, nv12.data());
        }
        return true;
    }

    // Sleep until `due_us` on the steady clock: the waitable timer to 1 ms
    // before, then yield. A plain Sleep(1) would wake up to 15 ms late
    // without timeBeginPeriod.
    void wait_until(uint64_t due_us) { wait_on(timer, due_us, true); }

    // `spin`: yield through the last millisecond for an exact wake. The
    // audio thread does not spin: its capture times are exact anyway.
    static void wait_on(HANDLE t, uint64_t due_us, bool spin) {
        for (;;) {
            const uint64_t now = steady_now_us();
            if (now >= due_us) return;
            const uint64_t left = due_us - now;
            if ((left > 1500 || !spin) && t) {
                const uint64_t sleep = spin ? left - 1000 : left;
                LARGE_INTEGER rel{};
                rel.QuadPart = -static_cast<LONGLONG>(sleep * 10);  // 100 ns units
                if (SetWaitableTimerEx(t, &rel, 0, nullptr, nullptr, nullptr, 0)) {
                    WaitForSingleObject(t, INFINITE);
                    continue;
                }
            }
            std::this_thread::yield();
        }
    }
};

ReferenceCapture::ReferenceCapture() : impl_(std::make_unique<Impl>()) {}

bool ReferenceCapture::provides_timed_audio() const { return !impl_->audio.empty(); }

void ReferenceCapture::set_timed_audio_callback(TimedAudioCallback cb) {
    std::lock_guard<std::mutex> lock(impl_->audio_cb_mutex);
    impl_->audio_cb = std::move(cb);
}

ReferenceCapture::~ReferenceCapture() {
    stop();
}

uint32_t ReferenceCapture::width() const { return impl_->clip.width; }
uint32_t ReferenceCapture::height() const { return impl_->clip.height; }

ReferenceInfo ReferenceCapture::info() const {
    std::lock_guard<std::mutex> lock(impl_->info_mutex);
    return impl_->info;
}

bool ReferenceCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    if (desc.mode != CaptureMode::Reference) {
        MELLO_LOG_ERROR(TAG, "initialize: not a reference source");
        return false;
    }
    Impl& m = *impl_;
    m.clip = desc.reference;
    if (m.clip.fps == 0) m.clip.fps = 60;

    m.file = CreateFileW(utf8_to_wide(m.clip.path).c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr,
                         OPEN_EXISTING, FILE_FLAG_SEQUENTIAL_SCAN, nullptr);
    if (m.file == INVALID_HANDLE_VALUE) {
        MELLO_LOG_ERROR(TAG, "cannot open clip '%s' (error %lu)", m.clip.path.c_str(), GetLastError());
        return false;
    }
    LARGE_INTEGER size{};
    GetFileSizeEx(m.file, &size);
    m.frames = reference_clip_frames(static_cast<uint64_t>(size.QuadPart), m.clip.width, m.clip.height);
    if (m.frames == 0) {
        MELLO_LOG_ERROR(TAG, "clip '%s' (%lld bytes) is not a whole number of %ux%u 4:2:0 frames",
                        m.clip.path.c_str(), static_cast<long long>(size.QuadPart), m.clip.width,
                        m.clip.height);
        return false;
    }
    m.frame_bytes = reference_frame_bytes(m.clip.width, m.clip.height);
    m.raw.resize(m.frame_bytes);
    m.nv12.resize(m.frame_bytes);

    // Stage 7: the clip's audio, one loop long at 48 kHz stereo.
    m.audio.clear();
    if (!m.clip.audio_path.empty()) {
        HANDLE af = CreateFileW(utf8_to_wide(m.clip.audio_path).c_str(), GENERIC_READ,
                                FILE_SHARE_READ, nullptr, OPEN_EXISTING, 0, nullptr);
        if (af == INVALID_HANDLE_VALUE) {
            MELLO_LOG_ERROR(TAG, "cannot open clip audio '%s' (error %lu)",
                            m.clip.audio_path.c_str(), GetLastError());
            return false;
        }
        const uint64_t loop_frames = m.frames * 48'000ULL / m.clip.fps;
        std::vector<int16_t> pcm(static_cast<size_t>(loop_frames * 2), 0);
        DWORD got = 0;
        const DWORD want = static_cast<DWORD>(pcm.size() * sizeof(int16_t));
        const BOOL ok = ReadFile(af, pcm.data(), want, &got, nullptr);
        CloseHandle(af);
        if (!ok) {
            MELLO_LOG_ERROR(TAG, "cannot read clip audio '%s'", m.clip.audio_path.c_str());
            return false;
        }
        m.audio.resize(pcm.size());
        for (size_t i = 0; i < pcm.size(); ++i) m.audio[i] = static_cast<float>(pcm[i]) / 32768.0f;
        MELLO_LOG_INFO(TAG, "clip audio '%s': %llu of %llu frames (48 kHz stereo)",
                       m.clip.audio_path.c_str(), static_cast<unsigned long long>(got / 4),
                       static_cast<unsigned long long>(loop_frames));
    }

    m.device = device.d3d11();
    m.device->GetImmediateContext(&m.context);
    HRESULT hr = m.device.As(&m.video_device);
    if (SUCCEEDED(hr)) hr = m.context.As(&m.video_context);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "no D3D11 video device/context: hr=0x%08X", hr);
        return false;
    }

    D3D11_TEXTURE2D_DESC td{};
    td.Width = m.clip.width;
    td.Height = m.clip.height;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = DXGI_FORMAT_NV12;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET;
    hr = m.device->CreateTexture2D(&td, nullptr, &m.nv12_tex);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "NV12 upload texture failed: hr=0x%08X", hr);
        return false;
    }

    D3D11_VIDEO_PROCESSOR_CONTENT_DESC cd{};
    cd.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    cd.InputWidth = m.clip.width;
    cd.InputHeight = m.clip.height;
    cd.OutputWidth = m.clip.width;
    cd.OutputHeight = m.clip.height;
    cd.Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;
    hr = m.video_device->CreateVideoProcessorEnumerator(&cd, &m.vp_enum);
    if (SUCCEEDED(hr)) hr = m.video_device->CreateVideoProcessor(m.vp_enum.Get(), 0, &m.vp);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "video processor failed: hr=0x%08X", hr);
        return false;
    }
    // Input: BT.709 studio-range YCbCr, as the clip is written. Output:
    // full-range RGB, as a desktop or game frame. The preprocessor applies
    // the inverse, so the encoder sees the clip's values up to rounding.
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE in_cs{};
    in_cs.Usage = 0;
    in_cs.RGB_Range = 0;
    in_cs.YCbCr_Matrix = 1;   // BT.709
    in_cs.Nominal_Range = 1;  // 16-235
    m.video_context->VideoProcessorSetStreamColorSpace(m.vp.Get(), 0, &in_cs);
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE out_cs{};
    out_cs.Usage = 0;
    out_cs.RGB_Range = 0;     // full range 0-255
    out_cs.YCbCr_Matrix = 1;
    out_cs.Nominal_Range = 2;
    m.video_context->VideoProcessorSetOutputColorSpace(m.vp.Get(), &out_cs);
    m.video_context->VideoProcessorSetStreamAutoProcessingMode(m.vp.Get(), 0, FALSE);

    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivd{};
    ivd.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
    hr = m.video_device->CreateVideoProcessorInputView(m.nv12_tex.Get(), m.vp_enum.Get(), &ivd, &m.input_view);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "input view failed: hr=0x%08X", hr);
        return false;
    }

    for (size_t i = 0; i < Impl::kSlots; ++i) {
        D3D11_TEXTURE2D_DESC bd = td;
        bd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        bd.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
        hr = m.device->CreateTexture2D(&bd, nullptr, &m.bgra[i]);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "BGRA slot %zu failed: hr=0x%08X", i, hr);
            return false;
        }
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC ovd{};
        ovd.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
        hr = m.video_device->CreateVideoProcessorOutputView(m.bgra[i].Get(), m.vp_enum.Get(), &ovd,
                                                            &m.output_view[i]);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "output view %zu failed: hr=0x%08X", i, hr);
            return false;
        }
    }

    m.timer = CreateWaitableTimerExW(nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                                     TIMER_ALL_ACCESS);
    if (!m.timer) {
        // Older Windows: a normal waitable timer, still better than Sleep.
        m.timer = CreateWaitableTimerExW(nullptr, nullptr, 0, TIMER_ALL_ACCESS);
    }
    if (!m.audio.empty()) {
        m.audio_timer = CreateWaitableTimerExW(nullptr, nullptr,
                                               CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                                               TIMER_ALL_ACCESS);
        if (!m.audio_timer) {
            m.audio_timer = CreateWaitableTimerExW(nullptr, nullptr, 0, TIMER_ALL_ACCESS);
        }
    }

    {
        std::lock_guard<std::mutex> lock(m.info_mutex);
        m.info = {};
        m.info.clip_frames = static_cast<uint32_t>(m.frames);
        m.info.fps = m.clip.fps;
    }
    MELLO_LOG_INFO(TAG, "clip '%s': %ux%u %s, %llu frames at %u fps", m.clip.path.c_str(),
                   m.clip.width, m.clip.height, m.clip.nv12 ? "NV12" : "I420",
                   static_cast<unsigned long long>(m.frames), m.clip.fps);
    return true;
}

bool ReferenceCapture::start(uint32_t target_fps, FrameCallback callback) {
    Impl& m = *impl_;
    if (m.running.load() || !callback || !m.vp) return false;
    if (target_fps != 0 && target_fps != m.clip.fps) {
        // The pipeline decimates to its own rate; the clip plays at its rate.
        MELLO_LOG_INFO(TAG, "pipeline asks %u fps; the clip plays at %u fps", target_fps, m.clip.fps);
    }
    if (!m.read_frame(0)) {
        MELLO_LOG_ERROR(TAG, "cannot read frame 0");
        return false;
    }
    m.running = true;
    m.thread = std::thread([this, callback = std::move(callback)]() {
        Impl& s = *impl_;
        // A little room for the first upload, then a fixed schedule.
        const uint64_t start = steady_now_us() + 20'000;
        {
            std::lock_guard<std::mutex> lock(s.info_mutex);
            s.info.start_us = start;
        }
        if (!s.audio.empty() && !s.audio_thread.joinable()) {
            // Stage 7: the clip's audio on the same clock, frame 0 at start.
            s.audio_thread = std::thread([&s, start]() {
                constexpr uint32_t kChunk = 480;  // 10 ms
                const size_t loop = s.audio.size() / 2;
                std::vector<float> chunk(kChunk * 2);
                uint64_t j = 0;
                while (s.running.load(std::memory_order_relaxed)) {
                    const uint64_t captured = start + j * 10'000;
                    // Handed over when complete, as a capture device does.
                    Impl::wait_on(s.audio_timer, captured + 10'000, false);
                    if (!s.running.load(std::memory_order_relaxed)) break;
                    for (uint32_t i = 0; i < kChunk; ++i) {
                        const size_t f = static_cast<size_t>((j * kChunk + i) % loop);
                        chunk[i * 2] = s.audio[f * 2];
                        chunk[i * 2 + 1] = s.audio[f * 2 + 1];
                    }
                    {
                        std::lock_guard<std::mutex> lock(s.audio_cb_mutex);
                        if (s.audio_cb) {
                            s.audio_cb(chunk.data(), kChunk, 2, 48'000,
                                       static_cast<int64_t>(captured));
                        }
                    }
                    ++j;
                }
            });
        }
        uint64_t n = 0;
        while (s.running.load(std::memory_order_relaxed)) {
            const uint64_t due = reference_due_us(start, n, s.clip.fps);
            s.wait_until(due);
            if (!s.running.load(std::memory_order_relaxed)) break;

            const size_t slot = static_cast<size_t>(n % Impl::kSlots);
            s.context->UpdateSubresource(s.nv12_tex.Get(), 0, nullptr, s.nv12.data(), s.clip.width,
                                         static_cast<UINT>(s.nv12.size()));
            D3D11_VIDEO_PROCESSOR_STREAM stream{};
            stream.Enable = TRUE;
            stream.pInputSurface = s.input_view.Get();
            const HRESULT hr =
                s.video_context->VideoProcessorBlt(s.vp.Get(), s.output_view[slot].Get(), 0, 1, &stream);
            if (FAILED(hr)) {
                MELLO_LOG_ERROR(TAG, "VideoProcessorBlt failed: hr=0x%08X; source stops", hr);
                failed_.store(true, std::memory_order_relaxed);
                break;
            }
            const uint64_t handed = steady_now_us();
            callback(s.bgra[slot].Get(), due);
            {
                std::lock_guard<std::mutex> lock(s.info_mutex);
                s.info.frames_delivered = n + 1;
                const uint64_t late = handed > due ? handed - due : 0;
                if (late > 2000) s.info.late_frames++;
                s.info.max_late_us = std::max(s.info.max_late_us, late);
                s.info.loops = static_cast<uint32_t>((n + 1) / s.frames);
            }
            ++n;
            // Read the next frame now, inside this frame's interval.
            if (!s.read_frame(n % s.frames)) {
                MELLO_LOG_ERROR(TAG, "cannot read frame %llu; source stops",
                                static_cast<unsigned long long>(n % s.frames));
                failed_.store(true, std::memory_order_relaxed);
                break;
            }
        }
    });
    return true;
}

void ReferenceCapture::stop() {
    Impl& m = *impl_;
    m.running = false;
    if (m.timer) {
        // Wake a sleeping thread at once.
        LARGE_INTEGER now{};
        now.QuadPart = -1;
        SetWaitableTimerEx(m.timer, &now, 0, nullptr, nullptr, nullptr, 0);
    }
    if (m.thread.joinable()) m.thread.join();
    if (m.audio_timer) {
        LARGE_INTEGER now{};
        now.QuadPart = -1;
        SetWaitableTimerEx(m.audio_timer, &now, 0, nullptr, nullptr, nullptr, 0);
    }
    if (m.audio_thread.joinable()) m.audio_thread.join();
}

#endif

} // namespace mello::video
