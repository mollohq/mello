#ifdef _WIN32
#include "video_preprocessor.hpp"
#include "../util/log.hpp"
#include <cstdio>
#include <cstring>

namespace mello::video {

static constexpr const char* TAG = "video/color";

static void save_bmp(const char* path, const uint8_t* pixels, uint32_t w, uint32_t h,
                     uint32_t src_pitch, bool is_bgra) {
    FILE* f = fopen(path, "wb");
    if (!f) return;

    uint32_t row_bytes = w * 4;
    uint32_t img_size  = row_bytes * h;
    uint32_t file_size = 54 + img_size;

    uint8_t hdr[54]{};
    hdr[0] = 'B'; hdr[1] = 'M';
    memcpy(hdr + 2, &file_size, 4);
    uint32_t off = 54; memcpy(hdr + 10, &off, 4);
    uint32_t dib = 40;  memcpy(hdr + 14, &dib, 4);
    memcpy(hdr + 18, &w, 4);
    int32_t neg_h = -(int32_t)h;
    memcpy(hdr + 22, &neg_h, 4);
    uint16_t planes = 1; memcpy(hdr + 26, &planes, 2);
    uint16_t bpp = 32;   memcpy(hdr + 28, &bpp, 2);
    memcpy(hdr + 34, &img_size, 4);
    fwrite(hdr, 1, 54, f);

    for (uint32_t y = 0; y < h; ++y) {
        const uint8_t* row = pixels + y * src_pitch;
        if (is_bgra) {
            fwrite(row, 1, row_bytes, f);
        } else {
            for (uint32_t x = 0; x < w; ++x) {
                uint8_t bgra[4] = { row[x*4+2], row[x*4+1], row[x*4+0], row[x*4+3] };
                fwrite(bgra, 1, 4, f);
            }
        }
    }
    fclose(f);
    MELLO_LOG_INFO(TAG, "Saved debug frame: %s (%ux%u)", path, w, h);
}

VideoPreprocessor::~VideoPreprocessor() {
    shutdown();
}

bool VideoPreprocessor::initialize(const GraphicsDevice& device, uint32_t width, uint32_t height) {
    return initialize(device, width, height, width, height);
}

bool VideoPreprocessor::initialize(const GraphicsDevice& device,
                                uint32_t in_w, uint32_t in_h,
                                uint32_t out_w, uint32_t out_h) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    width_  = out_w;
    height_ = out_h;

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

    // NV12 output ring: 3 slots so capture and encode can overlap. The
    // textures live as long as the preprocessor: the encoder registers each
    // one once, and a source resize must not invalidate that.
    for (size_t i = 0; i < NV12_RING_SLOTS; ++i) {
        if (!create_nv12_slot(i, out_w, out_h)) return false;
    }
    nv12_write_idx_ = 0;

    if (!build_processor(in_w, in_h)) return false;

    if (in_w != out_w || in_h != out_h) {
        MELLO_LOG_INFO(TAG, "VideoPreprocessor initialized: %ux%u -> %ux%u BGRA->NV12 (GPU video processor, downscale)",
            in_w, in_h, out_w, out_h);
    } else {
        MELLO_LOG_INFO(TAG, "VideoPreprocessor initialized: %ux%u BGRA->NV12 (GPU video processor)", out_w, out_h);
    }
    return true;
}

FitRect VideoPreprocessor::fit(uint32_t src_w, uint32_t src_h, uint32_t dst_w, uint32_t dst_h) {
    FitRect full{0, 0, dst_w, dst_h};
    if (src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0) return full;

    // Compare the aspects as cross products, so no rounding enters here.
    const uint64_t src_wide = static_cast<uint64_t>(src_w) * dst_h;
    const uint64_t dst_wide = static_cast<uint64_t>(dst_w) * src_h;
    const uint64_t larger = src_wide > dst_wide ? src_wide : dst_wide;
    const uint64_t smaller = src_wide > dst_wide ? dst_wide : src_wide;
    if ((larger - smaller) * 100 < larger) return full;

    FitRect out{};
    if (src_wide > dst_wide) {
        // Wider than the output: full width, bars above and below.
        out.width = dst_w;
        out.height = static_cast<uint32_t>(static_cast<uint64_t>(dst_w) * src_h / src_w) & ~1u;
        if (out.height == 0) out.height = 2;
        out.x = 0;
        out.y = ((dst_h - out.height) / 2) & ~1u;
    } else {
        // Narrower: full height, bars left and right.
        out.height = dst_h;
        out.width = static_cast<uint32_t>(static_cast<uint64_t>(dst_h) * src_w / src_h) & ~1u;
        if (out.width == 0) out.width = 2;
        out.y = 0;
        out.x = ((dst_w - out.width) / 2) & ~1u;
    }
    return out;
}

bool VideoPreprocessor::build_processor(uint32_t in_w, uint32_t in_h) {
    video_processor_.Reset();
    vp_enum_.Reset();
    for (auto& slot : nv12_slots_) slot.output_view.Reset();

    D3D11_VIDEO_PROCESSOR_CONTENT_DESC content_desc{};
    content_desc.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    content_desc.InputWidth  = in_w;
    content_desc.InputHeight = in_h;
    content_desc.OutputWidth  = width_;
    content_desc.OutputHeight = height_;
    content_desc.Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;

    HRESULT hr = video_device_->CreateVideoProcessorEnumerator(&content_desc, &vp_enum_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateVideoProcessorEnumerator failed: hr=0x%08X", hr);
        return false;
    }

    hr = video_device_->CreateVideoProcessor(vp_enum_.Get(), 0, &video_processor_);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateVideoProcessor failed: hr=0x%08X", hr);
        return false;
    }

    // Pin input/output color spaces so the decode side can match exactly.
    // Input: full-range RGB (desktop capture). Output: BT.709 studio-swing NV12.
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE input_cs{};
    input_cs.Usage         = 0; // playback
    input_cs.RGB_Range     = 0; // full range (0-255)
    input_cs.YCbCr_Matrix  = 1; // BT.709
    input_cs.Nominal_Range = 2; // 0-255
    video_context_->VideoProcessorSetStreamColorSpace(video_processor_.Get(), 0, &input_cs);

    D3D11_VIDEO_PROCESSOR_COLOR_SPACE output_cs{};
    output_cs.Usage         = 0;
    output_cs.RGB_Range     = 0;
    output_cs.YCbCr_Matrix  = 1; // BT.709
    output_cs.Nominal_Range = 1; // 16-235 (studio swing)
    video_context_->VideoProcessorSetOutputColorSpace(video_processor_.Get(), &output_cs);

    // Disable all auto-processing that could alter colors
    video_context_->VideoProcessorSetStreamAutoProcessingMode(video_processor_.Get(), 0, FALSE);

    // The whole source, into the part of the output that keeps its aspect.
    // The rectangles are always explicit: a fixed source rectangle cropped a
    // source that grew mid-stream.
    const FitRect dst = fit(in_w, in_h, width_, height_);
    RECT src_rect = {0, 0, static_cast<LONG>(in_w), static_cast<LONG>(in_h)};
    RECT dst_rect = {static_cast<LONG>(dst.x), static_cast<LONG>(dst.y),
                     static_cast<LONG>(dst.x + dst.width), static_cast<LONG>(dst.y + dst.height)};
    RECT out_rect = {0, 0, static_cast<LONG>(width_), static_cast<LONG>(height_)};
    video_context_->VideoProcessorSetStreamSourceRect(video_processor_.Get(), 0, TRUE, &src_rect);
    video_context_->VideoProcessorSetStreamDestRect(video_processor_.Get(), 0, TRUE, &dst_rect);
    video_context_->VideoProcessorSetOutputTargetRect(video_processor_.Get(), TRUE, &out_rect);
    // The bars outside the picture are black. The colour is given in the
    // output space, studio-range BT.709: Y 16, Cb and Cr 128. Given as RGB
    // black, the NVIDIA driver wrote Y 0, below black, which a decoder clips
    // and an encoder spends bits on.
    D3D11_VIDEO_COLOR black{};
    black.YCbCr.Y  = 16.0f / 255.0f;
    black.YCbCr.Cb = 128.0f / 255.0f;
    black.YCbCr.Cr = 128.0f / 255.0f;
    black.YCbCr.A  = 1.0f;
    video_context_->VideoProcessorSetOutputBackgroundColor(video_processor_.Get(), TRUE, &black);

    for (size_t i = 0; i < NV12_RING_SLOTS; ++i) {
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC out_desc{};
        out_desc.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
        out_desc.Texture2D.MipSlice = 0;
        hr = video_device_->CreateVideoProcessorOutputView(
            nv12_slots_[i].texture.Get(), vp_enum_.Get(), &out_desc, &nv12_slots_[i].output_view);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "CreateVideoProcessorOutputView slot %zu failed: hr=0x%08X", i, hr);
            return false;
        }
    }

    in_w_ = in_w;
    in_h_ = in_h;
    return true;
}

bool VideoPreprocessor::create_nv12_slot(size_t idx, uint32_t w, uint32_t h) {
    D3D11_TEXTURE2D_DESC nv12_desc{};
    nv12_desc.Width  = w;
    nv12_desc.Height = h;
    nv12_desc.MipLevels = 1;
    nv12_desc.ArraySize = 1;
    nv12_desc.Format = DXGI_FORMAT_NV12;
    nv12_desc.SampleDesc.Count = 1;
    nv12_desc.Usage = D3D11_USAGE_DEFAULT;
    nv12_desc.BindFlags = D3D11_BIND_RENDER_TARGET;

    HRESULT hr = device_->CreateTexture2D(&nv12_desc, nullptr, &nv12_slots_[idx].texture);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "Failed to create NV12 texture slot %zu: hr=0x%08X", idx, hr);
        return false;
    }
    return true;
}

ConvertResult VideoPreprocessor::convert(ID3D11Texture2D* bgra_source) {
    D3D11_TEXTURE2D_DESC src_desc{};
    bgra_source->GetDesc(&src_desc);

    if (frame_count_ == 0) {
        MELLO_LOG_DEBUG(TAG, "convert(): input tex fmt=%u %ux%u bindFlags=0x%X usage=%u misc=0x%X",
            src_desc.Format, src_desc.Width, src_desc.Height,
            src_desc.BindFlags, src_desc.Usage, src_desc.MiscFlags);
    }

    // A new source size: the whole new picture scales into the same output.
    bool resized = false;
    if (src_desc.Width != in_w_ || src_desc.Height != in_h_) {
        const uint32_t old_w = in_w_, old_h = in_h_;
        if (!build_processor(src_desc.Width, src_desc.Height)) {
            MELLO_LOG_ERROR(TAG, "retarget to a %ux%u source failed", src_desc.Width,
                            src_desc.Height);
            return {nullptr, nv12_write_idx_, false};
        }
        const FitRect dst = fit(in_w_, in_h_, width_, height_);
        MELLO_LOG_INFO(TAG, "source %ux%u -> %ux%u; scaled into %ux%u at (%u,%u) of %ux%u",
                       old_w, old_h, in_w_, in_h_, dst.width, dst.height, dst.x, dst.y,
                       width_, height_);
        resized = true;
    }

    size_t slot = nv12_write_idx_;
    nv12_write_idx_ = (nv12_write_idx_ + 1) % NV12_RING_SLOTS;
    auto& s = nv12_slots_[slot];

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
        return {nullptr, slot, resized};
    }

    D3D11_VIDEO_PROCESSOR_STREAM stream{};
    stream.Enable = TRUE;
    stream.pInputSurface = input_view.Get();

    hr = video_context_->VideoProcessorBlt(
        video_processor_.Get(), s.output_view.Get(), 0, 1, &stream);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "VideoProcessorBlt failed: hr=0x%08X", hr);
        return {nullptr, slot, resized};
    }

    if (frame_count_ < 3) {
        verify_nv12_output(bgra_source, slot);
    }
    frame_count_++;

    return {s.texture.Get(), slot, resized};
}

void VideoPreprocessor::verify_nv12_output(ID3D11Texture2D* bgra_source, size_t slot) {
    auto* nv12_tex = nv12_slots_[slot].texture.Get();
    D3D11_TEXTURE2D_DESC desc{};
    nv12_tex->GetDesc(&desc);

    D3D11_TEXTURE2D_DESC staging_desc = desc;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

    ComPtr<ID3D11Texture2D> staging;
    HRESULT hr = device_->CreateTexture2D(&staging_desc, nullptr, &staging);
    if (FAILED(hr)) return;

    context_->CopyResource(staging.Get(), nv12_tex);

    D3D11_MAPPED_SUBRESOURCE mapped{};
    hr = context_->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped);
    if (FAILED(hr)) return;

    const uint8_t* data = static_cast<const uint8_t*>(mapped.pData);
    uint32_t pitch = mapped.RowPitch;

    // Stage the BGRA source
    D3D11_TEXTURE2D_DESC src_desc{};
    bgra_source->GetDesc(&src_desc);

    D3D11_TEXTURE2D_DESC bgra_stg_desc = src_desc;
    bgra_stg_desc.Usage = D3D11_USAGE_STAGING;
    bgra_stg_desc.BindFlags = 0;
    bgra_stg_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    bgra_stg_desc.MiscFlags = 0;

    ComPtr<ID3D11Texture2D> bgra_staging;
    hr = device_->CreateTexture2D(&bgra_stg_desc, nullptr, &bgra_staging);
    bool have_bgra = false;
    D3D11_MAPPED_SUBRESOURCE bgra_mapped{};
    if (SUCCEEDED(hr)) {
        context_->CopyResource(bgra_staging.Get(), bgra_source);
        hr = context_->Map(bgra_staging.Get(), 0, D3D11_MAP_READ, 0, &bgra_mapped);
        have_bgra = SUCCEEDED(hr);
    }

    // Sample several positions: center, and scan UV for max-chroma pixel
    struct SamplePoint { uint32_t x, y; const char* name; };
    SamplePoint points[] = {
        { width_ / 2, height_ / 2, "center" },
        { width_ / 4, height_ / 4, "q1" },
        { 3 * width_ / 4, height_ / 4, "q2" },
    };

    for (auto& pt : points) {
        uint8_t y_val = data[pt.y * pitch + pt.x];
        const uint8_t* uv_row = data + pitch * height_ + (pt.y / 2) * pitch;
        uint8_t u_val = uv_row[(pt.x & ~1u)];
        uint8_t v_val = uv_row[(pt.x & ~1u) + 1];

        if (have_bgra && pt.x < src_desc.Width && pt.y < src_desc.Height) {
            const uint8_t* bgra_row = static_cast<const uint8_t*>(bgra_mapped.pData) + pt.y * bgra_mapped.RowPitch;
            uint8_t b = bgra_row[pt.x * 4 + 0];
            uint8_t g = bgra_row[pt.x * 4 + 1];
            uint8_t r = bgra_row[pt.x * 4 + 2];

            // Expected NV12 under BT.709 (studio-swing)
            float rf = r / 255.0f, gf = g / 255.0f, bf = b / 255.0f;
            int y709 = (int)(16.0f + 219.0f * (0.2126f * rf + 0.7152f * gf + 0.0722f * bf) + 0.5f);
            int u709 = (int)(128.0f + 224.0f * ((bf - (0.2126f * rf + 0.7152f * gf + 0.0722f * bf)) / 1.8556f) + 0.5f);
            int v709 = (int)(128.0f + 224.0f * ((rf - (0.2126f * rf + 0.7152f * gf + 0.0722f * bf)) / 1.5748f) + 0.5f);

            // Expected NV12 under BT.601 (studio-swing)
            int y601 = (int)(16.0f + 219.0f * (0.299f * rf + 0.587f * gf + 0.114f * bf) + 0.5f);
            int u601 = (int)(128.0f + 224.0f * ((bf - (0.299f * rf + 0.587f * gf + 0.114f * bf)) / 1.772f) + 0.5f);
            int v601 = (int)(128.0f + 224.0f * ((rf - (0.299f * rf + 0.587f * gf + 0.114f * bf)) / 1.402f) + 0.5f);

            MELLO_LOG_DEBUG(TAG, "verify[%s] BGRA=(%u,%u,%u) -> NV12 Y=%u U=%u V=%u | expect709=(%d,%d,%d) expect601=(%d,%d,%d)",
                pt.name, r, g, b, y_val, u_val, v_val,
                y709, u709, v709, y601, u601, v601);
        } else {
            MELLO_LOG_DEBUG(TAG, "verify[%s] NV12 Y=%u U=%u V=%u",
                pt.name, y_val, u_val, v_val);
        }
    }

    if (have_bgra && frame_count_ == 0 && getenv("MELLO_DUMP_FRAMES")) {
        char path[256];
        snprintf(path, sizeof(path), "mello_host_frame_%llu.bmp", frame_count_);
        save_bmp(path, static_cast<const uint8_t*>(bgra_mapped.pData),
                 src_desc.Width, src_desc.Height, bgra_mapped.RowPitch, true);
    }

    context_->Unmap(staging.Get(), 0);
    if (have_bgra) context_->Unmap(bgra_staging.Get(), 0);
}

void VideoPreprocessor::shutdown() {
    for (auto& s : nv12_slots_) {
        s.output_view.Reset();
        s.texture.Reset();
    }
    video_processor_.Reset();
    vp_enum_.Reset();
    video_context_.Reset();
    video_device_.Reset();
    context_.Reset();
    device_.Reset();
}

} // namespace mello::video
#endif
