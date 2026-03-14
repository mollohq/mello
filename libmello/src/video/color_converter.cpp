#ifdef _WIN32
#include "color_converter.hpp"
#include "../util/log.hpp"

namespace mello::video {

static constexpr const char* TAG = "video/color";

ColorConverter::~ColorConverter() {
    shutdown();
}

bool ColorConverter::initialize(const GraphicsDevice& device, uint32_t width, uint32_t height) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    width_  = width;
    height_ = height;

    HRESULT hr = device_->QueryInterface(IID_PPV_ARGS(&video_device_));
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "QueryInterface(ID3D11VideoDevice) failed: hr=0x%08X", hr);
        return false;
    }

    hr = context_->QueryInterface(IID_PPV_ARGS(&video_context_));
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "QueryInterface(ID3D11VideoContext) failed: hr=0x%08X", hr);
        return false;
    }

    D3D11_VIDEO_PROCESSOR_CONTENT_DESC content_desc{};
    content_desc.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    content_desc.InputWidth  = width;
    content_desc.InputHeight = height;
    content_desc.OutputWidth  = width;
    content_desc.OutputHeight = height;
    content_desc.Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;

    hr = video_device_->CreateVideoProcessorEnumerator(&content_desc, &vp_enum_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateVideoProcessorEnumerator failed: hr=0x%08X", hr);
        return false;
    }

    hr = video_device_->CreateVideoProcessor(vp_enum_.Get(), 0, &video_processor_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateVideoProcessor failed: hr=0x%08X", hr);
        return false;
    }

    // NV12 output texture — used as video processor output and NVENC input
    D3D11_TEXTURE2D_DESC nv12_desc{};
    nv12_desc.Width  = width;
    nv12_desc.Height = height;
    nv12_desc.MipLevels = 1;
    nv12_desc.ArraySize = 1;
    nv12_desc.Format = DXGI_FORMAT_NV12;
    nv12_desc.SampleDesc.Count = 1;
    nv12_desc.Usage = D3D11_USAGE_DEFAULT;
    nv12_desc.BindFlags = D3D11_BIND_RENDER_TARGET;

    hr = device_->CreateTexture2D(&nv12_desc, nullptr, &nv12_texture_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Failed to create NV12 texture: hr=0x%08X", hr);
        return false;
    }

    // Output view on the NV12 texture
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC out_desc{};
    out_desc.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
    out_desc.Texture2D.MipSlice = 0;

    hr = video_device_->CreateVideoProcessorOutputView(
        nv12_texture_.Get(), vp_enum_.Get(), &out_desc, &output_view_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateVideoProcessorOutputView failed: hr=0x%08X", hr);
        return false;
    }

    MELLO_LOG_INFO(TAG, "Color converter initialized: %ux%u BGRA->NV12 (GPU video processor)", width, height);
    return true;
}

ID3D11Texture2D* ColorConverter::convert(ID3D11Texture2D* bgra_source) {
    D3D11_TEXTURE2D_DESC src_desc{};
    bgra_source->GetDesc(&src_desc);

    if (frame_count_ == 0) {
        MELLO_LOG_DEBUG(TAG, "convert(): input tex fmt=%u %ux%u bindFlags=0x%X usage=%u misc=0x%X",
            src_desc.Format, src_desc.Width, src_desc.Height,
            src_desc.BindFlags, src_desc.Usage, src_desc.MiscFlags);
    }

    // Create input view for this frame's BGRA texture
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC in_desc{};
    in_desc.FourCC = 0;
    in_desc.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
    in_desc.Texture2D.MipSlice = 0;

    ComPtr<ID3D11VideoProcessorInputView> input_view;
    HRESULT hr = video_device_->CreateVideoProcessorInputView(
        bgra_source, vp_enum_.Get(), &in_desc, &input_view);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateVideoProcessorInputView failed: hr=0x%08X (fmt=%u bind=0x%X)",
            hr, src_desc.Format, src_desc.BindFlags);
        return nullptr;
    }

    D3D11_VIDEO_PROCESSOR_STREAM stream{};
    stream.Enable = TRUE;
    stream.pInputSurface = input_view.Get();

    hr = video_context_->VideoProcessorBlt(
        video_processor_.Get(), output_view_.Get(), 0, 1, &stream);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "VideoProcessorBlt failed: hr=0x%08X", hr);
        return nullptr;
    }

    // Debug: on first frame, readback a few NV12 pixels to verify conversion
    if (frame_count_ < 3) {
        verify_nv12_output();
    }
    frame_count_++;

    return nv12_texture_.Get();
}

void ColorConverter::verify_nv12_output() {
    D3D11_TEXTURE2D_DESC desc{};
    nv12_texture_->GetDesc(&desc);

    D3D11_TEXTURE2D_DESC staging_desc = desc;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

    ComPtr<ID3D11Texture2D> staging;
    HRESULT hr = device_->CreateTexture2D(&staging_desc, nullptr, &staging);
    if (FAILED(hr)) {
        MELLO_LOG_WARN(TAG, "verify: CreateTexture2D staging failed: hr=0x%08X", hr);
        return;
    }

    context_->CopyResource(staging.Get(), nv12_texture_.Get());

    D3D11_MAPPED_SUBRESOURCE mapped{};
    hr = context_->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped);
    if (FAILED(hr)) {
        MELLO_LOG_WARN(TAG, "verify: Map failed: hr=0x%08X", hr);
        return;
    }

    const uint8_t* data = static_cast<const uint8_t*>(mapped.pData);
    uint32_t pitch = mapped.RowPitch;

    // Sample Y values from center of frame
    uint32_t cx = width_ / 2;
    uint32_t cy = height_ / 2;
    uint8_t y_tl = data[0];
    uint8_t y_center = data[cy * pitch + cx];
    uint8_t y_br = data[(height_ - 1) * pitch + (width_ - 1)];

    // UV plane starts after Y plane
    const uint8_t* uv_data = data + pitch * height_;
    uint8_t u_center = uv_data[(cy / 2) * pitch + (cx & ~1u)];
    uint8_t v_center = uv_data[(cy / 2) * pitch + (cx & ~1u) + 1];

    MELLO_LOG_DEBUG(TAG, "verify NV12: Y[0,0]=%u Y[center]=%u Y[br]=%u  UV[center]=(%u,%u)  pitch=%u",
        y_tl, y_center, y_br, u_center, v_center, pitch);

    context_->Unmap(staging.Get(), 0);
}

void ColorConverter::shutdown() {
    output_view_.Reset();
    video_processor_.Reset();
    vp_enum_.Reset();
    video_context_.Reset();
    video_device_.Reset();
    nv12_texture_.Reset();
    context_.Reset();
    device_.Reset();
}

} // namespace mello::video
#endif
