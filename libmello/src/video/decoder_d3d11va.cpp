#ifdef _WIN32
#include "decoder_d3d11va.hpp"
#include "../util/log.hpp"

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

static const GUID DXVA2_ModeH264_VLD_NoFGT =
    {0x1b81be68, 0xa0c7, 0x11d3, {0xb9, 0x84, 0x00, 0xc0, 0x4f, 0x2e, 0x73, 0xc5}};

bool D3d11vaDecoder::is_available(ID3D11Device* device) {
    if (!device) return false;

    Microsoft::WRL::ComPtr<ID3D11VideoDevice> video_dev;
    HRESULT hr = device->QueryInterface(IID_PPV_ARGS(&video_dev));
    if (FAILED(hr)) return false;

    UINT profile_count = video_dev->GetVideoDecoderProfileCount();
    for (UINT i = 0; i < profile_count; ++i) {
        GUID profile{};
        if (SUCCEEDED(video_dev->GetVideoDecoderProfile(i, &profile))) {
            if (IsEqualGUID(profile, DXVA2_ModeH264_VLD_NoFGT)) {
                return true;
            }
        }
    }
    return false;
}

bool D3d11vaDecoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    config_ = config;

    HRESULT hr = device_->QueryInterface(IID_PPV_ARGS(&video_device_));
    if (FAILED(hr)) {
        MELLO_LOG_DEBUG(TAG, "Probing D3D11VA... not available (QueryInterface failed)");
        return false;
    }

    hr = context_.As(&video_context_);
    if (FAILED(hr)) {
        MELLO_LOG_DEBUG(TAG, "Probing D3D11VA... not available (video context failed)");
        return false;
    }

    // Create decoder texture array (D3D11VA requires BIND_DECODER)
    D3D11_TEXTURE2D_DESC decode_desc{};
    decode_desc.Width  = config.width;
    decode_desc.Height = config.height;
    decode_desc.MipLevels = 1;
    decode_desc.ArraySize = 4; // Reference frames
    decode_desc.Format = DXGI_FORMAT_NV12;
    decode_desc.SampleDesc.Count = 1;
    decode_desc.Usage = D3D11_USAGE_DEFAULT;
    decode_desc.BindFlags = D3D11_BIND_DECODER;

    hr = device_->CreateTexture2D(&decode_desc, nullptr, &decode_tex_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "D3D11VA: Failed to create decode texture: hr=0x%08X", hr);
        return false;
    }

    // Create output view for subresource 0
    D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC view_desc{};
    view_desc.DecodeProfile = DXVA2_ModeH264_VLD_NoFGT;
    view_desc.ViewDimension = D3D11_VDOV_DIMENSION_TEXTURE2D;
    view_desc.Texture2D.ArraySlice = 0;

    hr = video_device_->CreateVideoDecoderOutputView(
        decode_tex_.Get(), &view_desc, &output_view_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "D3D11VA: Failed to create output view: hr=0x%08X", hr);
        return false;
    }

    // Create the video decoder
    D3D11_VIDEO_DECODER_DESC dec_desc{};
    dec_desc.Guid = DXVA2_ModeH264_VLD_NoFGT;
    dec_desc.SampleWidth  = config.width;
    dec_desc.SampleHeight = config.height;
    dec_desc.OutputFormat = DXGI_FORMAT_NV12;

    D3D11_VIDEO_DECODER_CONFIG dec_config{};
    UINT config_count = 0;
    video_device_->GetVideoDecoderConfigCount(&dec_desc, &config_count);
    if (config_count > 0) {
        video_device_->GetVideoDecoderConfig(&dec_desc, 0, &dec_config);
    }

    hr = video_device_->CreateVideoDecoder(&dec_desc, &dec_config, &decoder_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "D3D11VA: CreateVideoDecoder failed: hr=0x%08X", hr);
        return false;
    }

    // Output texture (readable by staging/downstream)
    D3D11_TEXTURE2D_DESC frame_desc{};
    frame_desc.Width  = config.width;
    frame_desc.Height = config.height;
    frame_desc.MipLevels = 1;
    frame_desc.ArraySize = 1;
    frame_desc.Format = DXGI_FORMAT_NV12;
    frame_desc.SampleDesc.Count = 1;
    frame_desc.Usage = D3D11_USAGE_DEFAULT;
    frame_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;

    hr = device_->CreateTexture2D(&frame_desc, nullptr, &frame_tex_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "D3D11VA: Failed to create frame texture: hr=0x%08X", hr);
        return false;
    }

    MELLO_LOG_DEBUG(TAG, "Probing D3D11VA... ok");
    MELLO_LOG_INFO(TAG, "Selected decoder: D3D11VA codec=H264 resolution=%ux%u",
        config.width, config.height);
    return true;
}

void D3d11vaDecoder::shutdown() {
    decoder_.Reset();
    output_view_.Reset();
    decode_tex_.Reset();
    frame_tex_.Reset();
    video_context_.Reset();
    video_device_.Reset();
    context_.Reset();
    device_.Reset();
}

bool D3d11vaDecoder::submit_decode(const uint8_t* data, size_t size) {
    HRESULT hr = video_context_->DecoderBeginFrame(decoder_.Get(), output_view_.Get(), 0, nullptr);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "D3D11VA: DecoderBeginFrame failed: hr=0x%08X", hr);
        return false;
    }

    // Submit the compressed bitstream as a slice data buffer.
    // A full implementation would parse H.264 NAL units and fill:
    //   - DXVA_PicParams_H264 (picture parameters)
    //   - DXVA_Qmatrix_H264 (quantization matrix)
    //   - DXVA_Slice_H264_Short or DXVA_Slice_H264_Long (slice headers)
    //   - Raw slice data
    //
    // For now, we submit the raw bitstream as slice data. A proper H.264
    // NAL parser is needed for production use.
    D3D11_VIDEO_DECODER_BUFFER_DESC buf_desc{};
    buf_desc.BufferType = D3D11_VIDEO_DECODER_BUFFER_BITSTREAM;
    buf_desc.DataSize   = static_cast<UINT>(size);
    buf_desc.DataOffset = 0;

    UINT buf_size = 0;
    void* buf_ptr = nullptr;
    hr = video_context_->GetDecoderBuffer(decoder_.Get(),
        D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, &buf_size, &buf_ptr);
    if (SUCCEEDED(hr) && buf_ptr) {
        memcpy(buf_ptr, data, std::min(size, static_cast<size_t>(buf_size)));
        video_context_->ReleaseDecoderBuffer(decoder_.Get(), D3D11_VIDEO_DECODER_BUFFER_BITSTREAM);
    }

    hr = video_context_->SubmitDecoderBuffers(decoder_.Get(), 1, &buf_desc);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "D3D11VA: SubmitDecoderBuffers failed: hr=0x%08X", hr);
    }

    hr = video_context_->DecoderEndFrame(decoder_.Get());
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "D3D11VA: DecoderEndFrame failed: hr=0x%08X", hr);
        return false;
    }

    return true;
}

bool D3d11vaDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe) {
    if (!decoder_) return false;
    (void)is_keyframe;

    if (!submit_decode(data, size)) return false;

    // Copy decoded frame from decoder texture (array slice 0) to output
    context_->CopySubresourceRegion(
        frame_tex_.Get(), 0, 0, 0, 0,
        decode_tex_.Get(), D3D11CalcSubresource(0, 0, 1),
        nullptr);

    return true;
}

ID3D11Texture2D* D3d11vaDecoder::get_frame() {
    return frame_tex_.Get();
}

bool D3d11vaDecoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264;
}

} // namespace mello::video
#endif
