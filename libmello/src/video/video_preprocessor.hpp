#pragma once
#include "graphics_device.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <d3d11_1.h>
#include <wrl/client.h>
#include <array>

using Microsoft::WRL::ComPtr;

namespace mello::video {

struct ConvertResult {
    ID3D11Texture2D* texture;
    size_t           slot_index;
    /// True when this frame's source size differs from the previous frame's
    /// (or from the size at initialize). The preprocessor has retargeted.
    bool             source_resized = false;
};

/// Where a source picture lands inside the output.
struct FitRect {
    uint32_t x = 0;
    uint32_t y = 0;
    uint32_t width = 0;
    uint32_t height = 0;
};

class VideoPreprocessor {
public:
    static constexpr size_t NV12_RING_SLOTS = 3;

    VideoPreprocessor() = default;
    ~VideoPreprocessor();

    bool initialize(const GraphicsDevice& device, uint32_t width, uint32_t height);
    bool initialize(const GraphicsDevice& device,
                    uint32_t in_w, uint32_t in_h,
                    uint32_t out_w, uint32_t out_h);

    /// Convert BGRA source to the next NV12 ring slot. Returns texture + slot index.
    ///
    /// The source size can change from one frame to the next (a window
    /// resize, a display mode change, a ladder move). The output size never
    /// changes: the whole new source scales into it with its aspect kept
    /// (`fit`), and the rest of the output is black.
    ConvertResult convert(ID3D11Texture2D* bgra_source);

    /// The rectangle of a `src_w` x `src_h` picture scaled into a `dst_w` x
    /// `dst_h` output with its aspect kept, centred, on even coordinates.
    ///
    /// When the two aspects differ by less than 1 %, the picture fills the
    /// output: that is the rounding of `encode_size_for` (a width that is a
    /// multiple of 16), and a stretch that small is invisible where a black
    /// line of one or two pixels is not. Pure, testable without a GPU.
    static FitRect fit(uint32_t src_w, uint32_t src_h, uint32_t dst_w, uint32_t dst_h);

    uint32_t source_width() const { return in_w_; }
    uint32_t source_height() const { return in_h_; }

    void shutdown();

private:
    void verify_nv12_output(ID3D11Texture2D* bgra_source, size_t slot);
    bool create_nv12_slot(size_t idx, uint32_t w, uint32_t h);
    /// Builds the video processor for an input size and sets its rectangles.
    /// The NV12 ring keeps its textures (the encoder registered them); only
    /// their output views are made again.
    bool build_processor(uint32_t in_w, uint32_t in_h);

    ComPtr<ID3D11Device>                      device_;
    ComPtr<ID3D11DeviceContext>               context_;
    ComPtr<ID3D11VideoDevice>                 video_device_;
    ComPtr<ID3D11VideoContext>                video_context_;
    ComPtr<ID3D11VideoProcessorEnumerator>    vp_enum_;
    ComPtr<ID3D11VideoProcessor>              video_processor_;

    struct Nv12Slot {
        ComPtr<ID3D11Texture2D>                   texture;
        ComPtr<ID3D11VideoProcessorOutputView>    output_view;
    };
    std::array<Nv12Slot, NV12_RING_SLOTS> nv12_slots_{};
    size_t nv12_write_idx_ = 0;

    uint32_t in_w_  = 0;
    uint32_t in_h_  = 0;
    uint32_t width_  = 0;
    uint32_t height_ = 0;
    uint64_t frame_count_ = 0;
};

} // namespace mello::video
#endif
