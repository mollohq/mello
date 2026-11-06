#ifdef _WIN32
#include "decoder_mf.hpp"
#include "../util/log.hpp"

#include <windows.h>
#include <objbase.h>
#include <codecapi.h>
#include <d3d11.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mftransform.h>
#include <strmif.h>
#include <wmcodecdsp.h>

#include <algorithm>
#include <cstring>
#include <mutex>
#include <thread>

using Microsoft::WRL::ComPtr;

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

// One frame at 60 fps in 100 ns units. The transform needs increasing sample
// times; the viewer's timeline, not these times, decides when a picture shows.
static constexpr int64_t kFrameDuration100ns = 166'667;

// A stream change can repeat in one drain (the first SPS, then a new size).
// More than this in one access unit is a broken transform.
static constexpr int kMaxOutputsPerDrain = 8;

namespace {

bool env_is_one(const char* name) {
    // The process environment, not the C runtime's copy (decoder_factory.cpp
    // soft_decoder_requested has the reason).
    char buf[8] = {};
    const DWORD n = GetEnvironmentVariableA(name, buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
}

// Run `f` in the MTA. A thread with no COM apartment is in the implicit MTA
// while a CoIncrementMTAUsage cookie is held. An STA thread (the audio
// threads, a UI thread) hands the work to a short helper thread, so the
// transform never lives in an STA that the decode thread cannot call.
template <typename F>
void run_in_mta(F&& f) {
    APTTYPE type{};
    APTTYPEQUALIFIER qualifier{};
    const HRESULT hr = CoGetApartmentType(&type, &qualifier);
    if (SUCCEEDED(hr) && type == APTTYPE_MTA) {
        f();
        return;
    }
    std::thread helper([&f] { f(); });
    helper.join();
}

} // namespace

MfDecoder::MfDecoder() = default;

MfDecoder::~MfDecoder() {
    shutdown();
}

bool MfDecoder::is_available() {
    static std::once_flag once;
    static bool available = false;
    std::call_once(once, [] {
        CO_MTA_USAGE_COOKIE cookie = nullptr;
        if (FAILED(CoIncrementMTAUsage(&cookie))) {
            MELLO_LOG_WARN(TAG, "Media Foundation decoder: CoIncrementMTAUsage failed");
            return;
        }
        run_in_mta([] {
            const HRESULT start = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
            if (FAILED(start)) {
                MELLO_LOG_WARN(TAG, "Media Foundation decoder: MFStartup failed: hr=0x%08lX", start);
                return;
            }
            ComPtr<IMFTransform> mft;
            const HRESULT hr = CoCreateInstance(__uuidof(CMSH264DecoderMFT), nullptr, CLSCTX_INPROC_SERVER,
                                                IID_PPV_ARGS(&mft));
            if (hr == REGDB_E_CLASSNOTREG) {
                MELLO_LOG_WARN(TAG, "Media Foundation H.264 decoder is not installed: a Windows N edition "
                                    "needs the Media Feature Pack");
            } else if (FAILED(hr)) {
                MELLO_LOG_WARN(TAG, "Media Foundation H.264 decoder cannot be made: hr=0x%08lX", hr);
            } else {
                available = true;
            }
            mft.Reset();
            MFShutdown();
        });
        CoDecrementMTAUsage(cookie);
    });
    return available;
}

bool MfDecoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
    if (config.codec != VideoCodec::H264) return false;
    if (device.backend != GraphicsBackend::D3D11 || !device.handle) return false;
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    config_ = config;
    sample_time_ = 0;

    CO_MTA_USAGE_COOKIE cookie = nullptr;
    if (FAILED(CoIncrementMTAUsage(&cookie))) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decoder: CoIncrementMTAUsage failed");
        shutdown();
        return false;
    }
    mta_cookie_ = cookie;

    bool ok = false;
    run_in_mta([this, &ok] {
        const HRESULT hr = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "Media Foundation decoder: MFStartup failed: hr=0x%08lX", hr);
            return;
        }
        mf_started_ = true;
        ok = setup();
        if (!ok) release_com();
    });
    if (!ok) {
        shutdown();
        return false;
    }
    MELLO_LOG_INFO(TAG, "Selected decoder: MediaFoundation mode=%s codec=H264 resolution=%ux%u",
                   gpu_mode_ ? "gpu" : "cpu", config.width, config.height);
    return true;
}

bool MfDecoder::setup() {
    HRESULT hr = CoCreateInstance(__uuidof(CMSH264DecoderMFT), nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&mft_));
    if (FAILED(hr)) {
        MELLO_LOG_DEBUG(TAG, "Probing Media Foundation decoder... CoCreateInstance failed: hr=0x%08lX", hr);
        return false;
    }

    // Without low latency the transform holds pictures back for reordering,
    // and a picture shows only some access units after its own. Each of the
    // two settings alone turns it on (measured 2026-11-06); both are set, as
    // a transform build may honour only one of them.
    ComPtr<IMFAttributes> attrs;
    if (SUCCEEDED(mft_->GetAttributes(&attrs)) && attrs) {
        attrs->SetUINT32(MF_LOW_LATENCY, TRUE);
    }
    ComPtr<ICodecAPI> codec_api;
    if (SUCCEEDED(mft_.As(&codec_api))) {
        // Zero-filled in place of VariantInit, which needs oleaut32.
        VARIANT v{};
        v.vt = VT_UI4;
        v.ulVal = 1;
        hr = codec_api->SetValue(&CODECAPI_AVLowLatencyMode, &v);
        if (FAILED(hr)) MELLO_LOG_DEBUG(TAG, "Media Foundation decoder: AVLowLatencyMode refused: hr=0x%08lX", hr);
    }

    gpu_mode_ = false;
    if (env_is_one("MELLO_MF_DECODER_CPU")) {
        MELLO_LOG_INFO(TAG, "MELLO_MF_DECODER_CPU=1: Media Foundation decodes on the CPU");
    } else if (!(device_->GetCreationFlags() & D3D11_CREATE_DEVICE_VIDEO_SUPPORT)) {
        MELLO_LOG_INFO(TAG, "Media Foundation decoder: the device has no video support; decoding on the CPU");
    } else {
        UINT32 aware = 0;
        if (!attrs || FAILED(attrs->GetUINT32(MF_SA_D3D11_AWARE, &aware)) || !aware) {
            MELLO_LOG_INFO(TAG, "Media Foundation decoder is not D3D11 aware; decoding on the CPU");
        } else {
            UINT token = 0;
            hr = MFCreateDXGIDeviceManager(&token, &manager_);
            if (SUCCEEDED(hr)) hr = manager_->ResetDevice(device_.Get(), token);
            if (SUCCEEDED(hr)) {
                hr = mft_->ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER,
                                          reinterpret_cast<ULONG_PTR>(manager_.Get()));
            }
            if (SUCCEEDED(hr)) {
                gpu_mode_ = true;
            } else {
                MELLO_LOG_WARN(TAG, "Media Foundation decoder refused the D3D11 device: hr=0x%08lX; "
                                    "decoding on the CPU", hr);
                mft_->ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
                manager_.Reset();
            }
        }
    }

    ComPtr<IMFMediaType> in_type;
    hr = MFCreateMediaType(&in_type);
    if (SUCCEEDED(hr)) hr = in_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    if (SUCCEEDED(hr)) hr = in_type->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_H264);
    if (SUCCEEDED(hr)) hr = in_type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    if (SUCCEEDED(hr) && config_.width > 0 && config_.height > 0) {
        hr = MFSetAttributeSize(in_type.Get(), MF_MT_FRAME_SIZE, config_.width, config_.height);
    }
    if (SUCCEEDED(hr)) hr = mft_->SetInputType(0, in_type.Get(), 0);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decoder: H.264 input type refused: hr=0x%08lX", hr);
        return false;
    }
    if (!set_output_type()) return false;

    hr = mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    if (SUCCEEDED(hr)) hr = mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decoder: start of stream failed: hr=0x%08lX", hr);
        return false;
    }
    return true;
}

bool MfDecoder::set_output_type() {
    ComPtr<IMFMediaType> chosen;
    for (DWORD i = 0;; ++i) {
        ComPtr<IMFMediaType> type;
        const HRESULT hr = mft_->GetOutputAvailableType(0, i, &type);
        if (hr == MF_E_NO_MORE_TYPES) break;
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "Media Foundation decoder: GetOutputAvailableType failed: hr=0x%08lX", hr);
            return false;
        }
        GUID subtype{};
        if (SUCCEEDED(type->GetGUID(MF_MT_SUBTYPE, &subtype)) && subtype == MFVideoFormat_NV12) {
            chosen = type;
            break;
        }
    }
    if (!chosen) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decoder offers no NV12 output");
        return false;
    }
    HRESULT hr = mft_->SetOutputType(0, chosen.Get(), 0);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decoder: SetOutputType(NV12) failed: hr=0x%08lX", hr);
        return false;
    }

    UINT32 w = 0, h = 0;
    MFGetAttributeSize(chosen.Get(), MF_MT_FRAME_SIZE, &w, &h);
    coded_w_ = w;
    coded_h_ = h;

    // The SPS crop: 1080 visible rows in a 1088-row coded picture.
    MFVideoArea area{};
    UINT32 area_size = 0;
    if (SUCCEEDED(chosen->GetBlob(MF_MT_MINIMUM_DISPLAY_APERTURE, reinterpret_cast<UINT8*>(&area), sizeof(area),
                                  &area_size)) &&
        area_size == sizeof(area) && area.Area.cx > 0 && area.Area.cy > 0 && area.OffsetX.value >= 0 &&
        area.OffsetY.value >= 0) {
        visible_x_ = static_cast<uint32_t>(area.OffsetX.value);
        visible_y_ = static_cast<uint32_t>(area.OffsetY.value);
        visible_w_ = static_cast<uint32_t>(area.Area.cx);
        visible_h_ = static_cast<uint32_t>(area.Area.cy);
    } else {
        visible_x_ = 0;
        visible_y_ = 0;
        visible_w_ = w;
        visible_h_ = h;
    }
    // NV12 copies and boxes need even coordinates; the area stays inside the
    // coded picture.
    visible_x_ &= ~1u;
    visible_y_ &= ~1u;
    if (visible_x_ > coded_w_) visible_x_ = 0;
    if (visible_y_ > coded_h_) visible_y_ = 0;
    if (visible_w_ > coded_w_ - visible_x_) visible_w_ = coded_w_ - visible_x_;
    if (visible_h_ > coded_h_ - visible_y_) visible_h_ = coded_h_ - visible_y_;
    visible_w_ &= ~1u;
    visible_h_ &= ~1u;

    UINT32 stride = 0;
    cpu_stride_ = SUCCEEDED(chosen->GetUINT32(MF_MT_DEFAULT_STRIDE, &stride)) ? static_cast<int32_t>(stride)
                                                                              : static_cast<int32_t>(coded_w_);

    MFT_OUTPUT_STREAM_INFO info{};
    hr = mft_->GetOutputStreamInfo(0, &info);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decoder: GetOutputStreamInfo failed: hr=0x%08lX", hr);
        return false;
    }
    mft_provides_samples_ =
        (info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES)) != 0;
    cpu_out_size_ = std::max<DWORD>(info.cbSize, coded_w_ * coded_h_ * 3 / 2);
    cpu_out_sample_.Reset();

    MELLO_LOG_INFO(TAG, "Media Foundation output: NV12 coded=%ux%u visible=%ux%u at %u,%u mode=%s", coded_w_,
                   coded_h_, visible_w_, visible_h_, visible_x_, visible_y_, gpu_mode_ ? "gpu" : "cpu");
    return true;
}

bool MfDecoder::make_cpu_output_sample() {
    const DWORD size = cpu_out_size_;
    if (size == 0) return false;
    ComPtr<IMFSample> sample;
    ComPtr<IMFMediaBuffer> buffer;
    HRESULT hr = MFCreateSample(&sample);
    if (SUCCEEDED(hr)) hr = MFCreateMemoryBuffer(size, &buffer);
    if (SUCCEEDED(hr)) hr = sample->AddBuffer(buffer.Get());
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decoder: output sample of %lu bytes failed: hr=0x%08lX", size, hr);
        return false;
    }
    cpu_out_sample_ = sample;
    return true;
}

void MfDecoder::release_com() {
    if (mft_) mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    cpu_out_sample_.Reset();
    mft_.Reset();
    manager_.Reset();
    if (mf_started_) {
        MFShutdown();
        mf_started_ = false;
    }
}

void MfDecoder::shutdown() {
    if (mft_ || manager_ || cpu_out_sample_ || mf_started_) {
        run_in_mta([this] { release_com(); });
    }
    for (Slot& slot : slots_) slot = Slot{};
    nv12_buf_.clear();
    nv12_buf_.shrink_to_fit();
    context_.Reset();
    device_.Reset();
    gpu_mode_ = false;
    if (mta_cookie_) {
        CoDecrementMTAUsage(static_cast<CO_MTA_USAGE_COOKIE>(mta_cookie_));
        mta_cookie_ = nullptr;
    }
}

bool MfDecoder::drain_output(int slot_index, bool& frame_ready) {
    for (int i = 0; i < kMaxOutputsPerDrain; ++i) {
        if (!mft_provides_samples_) {
            if (!cpu_out_sample_ && !make_cpu_output_sample()) return false;
            // The transform writes the picture after the buffer's current
            // length. Without the reset, the used-again buffer keeps the
            // first picture in front, and the next one needs room for two
            // (E_FAIL from ProcessOutput when it has none). Measured on
            // msmpeg2vdec.dll, Windows 11 26300, 2026-11-06.
            ComPtr<IMFMediaBuffer> buffer;
            if (FAILED(cpu_out_sample_->GetBufferByIndex(0, &buffer)) || FAILED(buffer->SetCurrentLength(0))) {
                MELLO_LOG_ERROR(TAG, "Media Foundation decode: output buffer reset failed");
                return false;
            }
        }

        MFT_OUTPUT_DATA_BUFFER out{};
        out.dwStreamID = 0;
        out.pSample = mft_provides_samples_ ? nullptr : cpu_out_sample_.Get();
        DWORD status = 0;
        const HRESULT hr = mft_->ProcessOutput(0, 1, &out, &status);
        if (out.pEvents) out.pEvents->Release();
        // The transform's own sample: release it at once, so its texture
        // pool does not run dry.
        ComPtr<IMFSample> provided;
        if (mft_provides_samples_ && out.pSample) provided.Attach(out.pSample);

        if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) return true;
        if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
            // A new SPS (the first one, or a new size). The slots take the new
            // size at their next copy (ensure_slot).
            if (!set_output_type()) return false;
            continue;
        }
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "Media Foundation decode: ProcessOutput failed: hr=0x%08lX", hr);
            return false;
        }
        IMFSample* sample = mft_provides_samples_ ? provided.Get() : cpu_out_sample_.Get();
        if (!sample) {
            MELLO_LOG_ERROR(TAG, "Media Foundation decode: ProcessOutput gave no sample");
            return false;
        }
        // The newest picture wins the slot.
        if (!copy_to_slot(sample, slot_index)) return false;
        frame_ready = true;
    }
    MELLO_LOG_ERROR(TAG, "Media Foundation decode: more than %d outputs for one access unit", kMaxOutputsPerDrain);
    return false;
}

DecodeFeedResult MfDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe, int slot_index) {
    if (!mft_ || !data || size == 0 || slot_index < 0 || slot_index >= kOutputSlots) {
        return DecodeFeedResult::Error;
    }

    ComPtr<IMFMediaBuffer> buffer;
    HRESULT hr = MFCreateMemoryBuffer(static_cast<DWORD>(size), &buffer);
    BYTE* dst = nullptr;
    if (SUCCEEDED(hr)) hr = buffer->Lock(&dst, nullptr, nullptr);
    if (SUCCEEDED(hr)) {
        std::memcpy(dst, data, size);
        buffer->Unlock();
        hr = buffer->SetCurrentLength(static_cast<DWORD>(size));
    }
    ComPtr<IMFSample> sample;
    if (SUCCEEDED(hr)) hr = MFCreateSample(&sample);
    if (SUCCEEDED(hr)) hr = sample->AddBuffer(buffer.Get());
    if (SUCCEEDED(hr)) hr = sample->SetSampleTime(sample_time_);
    if (SUCCEEDED(hr)) hr = sample->SetSampleDuration(kFrameDuration100ns);
    if (SUCCEEDED(hr) && is_keyframe) hr = sample->SetUINT32(MFSampleExtension_CleanPoint, TRUE);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decode: input sample failed: hr=0x%08lX", hr);
        return DecodeFeedResult::Error;
    }
    sample_time_ += kFrameDuration100ns;

    bool frame_ready = false;
    hr = mft_->ProcessInput(0, sample.Get(), 0);
    if (hr == MF_E_NOTACCEPTING) {
        if (!drain_output(slot_index, frame_ready)) return DecodeFeedResult::Error;
        hr = mft_->ProcessInput(0, sample.Get(), 0);
    }
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decode: ProcessInput failed: hr=0x%08lX (keyframe=%d)", hr,
                        is_keyframe ? 1 : 0);
        return DecodeFeedResult::Error;
    }
    if (!drain_output(slot_index, frame_ready)) return DecodeFeedResult::Error;
    return frame_ready ? DecodeFeedResult::FrameReady : DecodeFeedResult::Accepted;
}

bool MfDecoder::ensure_slot(int slot_index, uint32_t w, uint32_t h) {
    Slot& slot = slots_[slot_index];
    if (slot.texture && slot.width == w && slot.height == h) return true;
    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = w;
    desc.Height = h;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_NV12;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    ComPtr<ID3D11Texture2D> texture;
    const HRESULT hr = device_->CreateTexture2D(&desc, nullptr, &texture);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decode: slot texture %ux%u failed: hr=0x%08lX", w, h, hr);
        return false;
    }
    slot.texture = texture;
    slot.width = w;
    slot.height = h;
    return true;
}

bool MfDecoder::copy_to_slot(IMFSample* sample, int slot_index) {
    const uint32_t w = visible_w_;
    const uint32_t h = visible_h_;
    if (w == 0 || h == 0) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decode: picture without a size");
        return false;
    }
    if (!ensure_slot(slot_index, w, h)) return false;
    ID3D11Texture2D* slot_tex = slots_[slot_index].texture.Get();

    ComPtr<IMFMediaBuffer> buffer;
    HRESULT hr = sample->GetBufferByIndex(0, &buffer);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Media Foundation decode: output sample has no buffer: hr=0x%08lX", hr);
        return false;
    }

    if (gpu_mode_) {
        ComPtr<IMFDXGIBuffer> dxgi;
        ComPtr<ID3D11Texture2D> texture;
        UINT subresource = 0;
        hr = buffer.As(&dxgi);
        if (SUCCEEDED(hr)) hr = dxgi->GetResource(IID_PPV_ARGS(&texture));
        if (SUCCEEDED(hr)) hr = dxgi->GetSubresourceIndex(&subresource);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "Media Foundation decode: output is not a D3D11 texture: hr=0x%08lX", hr);
            return false;
        }
        const D3D11_BOX box{visible_x_, visible_y_, 0, visible_x_ + w, visible_y_ + h, 1};
        context_->CopySubresourceRegion(slot_tex, 0, 0, 0, 0, texture.Get(), subresource, &box);
        return true;
    }

    // CPU: NV12 in system memory. The chroma plane starts after the coded
    // height of luma rows.
    ComPtr<IMF2DBuffer> buffer_2d;
    BYTE* scan0 = nullptr;
    LONG pitch = 0;
    DWORD length = 0;
    bool locked_2d = false;
    if (SUCCEEDED(buffer.As(&buffer_2d)) && SUCCEEDED(buffer_2d->Lock2D(&scan0, &pitch))) {
        locked_2d = true;
    } else {
        hr = buffer->Lock(&scan0, nullptr, &length);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "Media Foundation decode: output buffer lock failed: hr=0x%08lX", hr);
            return false;
        }
        pitch = cpu_stride_;
    }
    auto unlock = [&] {
        if (locked_2d) buffer_2d->Unlock2D();
        else buffer->Unlock();
    };
    const size_t needed = static_cast<size_t>(pitch) * coded_h_ * 3 / 2;
    if (pitch < static_cast<LONG>(visible_x_ + w) || (!locked_2d && length < needed)) {
        unlock();
        MELLO_LOG_ERROR(TAG, "Media Foundation decode: output buffer too small (pitch=%ld length=%lu)", pitch,
                        length);
        return false;
    }

    const uint8_t* y_plane = scan0;
    const uint8_t* uv_plane = scan0 + static_cast<size_t>(pitch) * coded_h_;
    nv12_buf_.resize(static_cast<size_t>(w) * h * 3 / 2);
    uint8_t* dst_y = nv12_buf_.data();
    uint8_t* dst_uv = dst_y + static_cast<size_t>(w) * h;
    for (uint32_t row = 0; row < h; ++row) {
        std::memcpy(dst_y + static_cast<size_t>(row) * w,
                    y_plane + static_cast<size_t>(visible_y_ + row) * pitch + visible_x_, w);
    }
    for (uint32_t row = 0; row < h / 2; ++row) {
        std::memcpy(dst_uv + static_cast<size_t>(row) * w,
                    uv_plane + static_cast<size_t>(visible_y_ / 2 + row) * pitch + visible_x_, w);
    }
    unlock();
    context_->UpdateSubresource(slot_tex, 0, nullptr, nv12_buf_.data(), w, static_cast<UINT>(nv12_buf_.size()));
    return true;
}

bool MfDecoder::picture(int slot_index, DecodedPicture& out) const {
    if (slot_index < 0 || slot_index >= kOutputSlots || !slots_[slot_index].texture) return false;
    const Slot& slot = slots_[slot_index];
    out.texture = slot.texture;
    out.format = DXGI_FORMAT_NV12;
    out.width = slot.width;
    out.height = slot.height;
    out.uv_row = 0;
    return true;
}

} // namespace mello::video
#endif
