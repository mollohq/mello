#include "decoder_videotoolbox.hpp"

#ifdef __APPLE__

#include "../util/log.hpp"
#import <VideoToolbox/VideoToolbox.h>
#import <CoreVideo/CoreVideo.h>

namespace mello::video {

static constexpr const char* TAG = "video/decoder-vt";

// H.264 NAL unit start code helpers
static const uint8_t* find_start_code(const uint8_t* p, const uint8_t* end) {
    while (p + 3 < end) {
        if (p[0] == 0 && p[1] == 0 && p[2] == 1) return p;
        if (p[0] == 0 && p[1] == 0 && p[2] == 0 && p + 3 < end && p[3] == 1) return p;
        ++p;
    }
    return end;
}

static size_t start_code_len(const uint8_t* p) {
    if (p[0] == 0 && p[1] == 0 && p[2] == 0 && p[3] == 1) return 4;
    return 3;
}

struct NALUnit {
    const uint8_t* data;
    size_t         size;
    uint8_t        type; // nal_unit_type (lower 5 bits)
};

static std::vector<NALUnit> parse_nalu_stream(const uint8_t* data, size_t size) {
    std::vector<NALUnit> units;
    const uint8_t* end = data + size;
    const uint8_t* p = find_start_code(data, end);

    while (p < end) {
        size_t sc = start_code_len(p);
        const uint8_t* nal_start = p + sc;
        const uint8_t* next = find_start_code(nal_start, end);

        if (nal_start < end) {
            NALUnit u;
            u.data = nal_start;
            u.size = next - nal_start;
            u.type = nal_start[0] & 0x1F;
            units.push_back(u);
        }
        p = next;
    }
    return units;
}

// VTDecompressionSession output callback
static void decompress_callback(
    void* context,
    void* source_ref,
    OSStatus status,
    VTDecodeInfoFlags flags,
    CVImageBufferRef image_buffer,
    CMTime pts,
    CMTime duration)
{
    (void)source_ref; (void)flags; (void)pts; (void)duration;

    auto* self = static_cast<VTDecoder*>(context);
    if (status != noErr || !image_buffer) {
        MELLO_LOG_WARN(TAG, "Decode callback: status=%d image_buffer=%p", (int)status, image_buffer);
        return;
    }

    CVPixelBufferRetain(image_buffer);

    std::lock_guard<std::mutex> lock(self->frame_mutex_);
    if (self->latest_frame_) {
        CVPixelBufferRelease((CVPixelBufferRef)self->latest_frame_);
    }
    self->latest_frame_ = image_buffer;
}

VTDecoder::VTDecoder() = default;

VTDecoder::~VTDecoder() {
    shutdown();
}

bool VTDecoder::is_available() {
    return true; // Always available on Apple Silicon
}

bool VTDecoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
    (void)device;
    width_  = config.width;
    height_ = config.height;
    // Session is created lazily on first keyframe (we need SPS/PPS to build the format description)
    MELLO_LOG_INFO(TAG, "VTDecoder initialized: %ux%u (session deferred until first keyframe)", width_, height_);
    return true;
}

void VTDecoder::shutdown() {
    if (session_) {
        VTDecompressionSessionInvalidate((VTDecompressionSessionRef)session_);
        CFRelease(session_);
        session_ = nullptr;
    }
    if (format_) {
        CFRelease(format_);
        format_ = nullptr;
    }
    std::lock_guard<std::mutex> lock(frame_mutex_);
    if (latest_frame_) {
        CVPixelBufferRelease((CVPixelBufferRef)latest_frame_);
        latest_frame_ = nullptr;
    }
}

bool VTDecoder::create_format_description(const uint8_t* sps, size_t sps_len,
                                           const uint8_t* pps, size_t pps_len) {
    if (format_) {
        CFRelease(format_);
        format_ = nullptr;
    }

    const uint8_t* params[2] = { sps, pps };
    const size_t   sizes[2]  = { sps_len, pps_len };

    CMVideoFormatDescriptionRef fmt = nullptr;
    OSStatus status = CMVideoFormatDescriptionCreateFromH264ParameterSets(
        kCFAllocatorDefault,
        2, params, sizes,
        4, // NAL length size (AVCC format uses 4-byte length prefix)
        &fmt);

    if (status != noErr) {
        MELLO_LOG_ERROR(TAG, "CMVideoFormatDescriptionCreateFromH264ParameterSets failed: %d", (int)status);
        return false;
    }

    format_ = (void*)fmt; // CMVideoFormatDescriptionRef is const-qualified; cast to store

    CMVideoDimensions dims = CMVideoFormatDescriptionGetDimensions(fmt);
    MELLO_LOG_INFO(TAG, "Format description created: %dx%d", dims.width, dims.height);
    width_  = dims.width;
    height_ = dims.height;
    return true;
}

bool VTDecoder::create_session() {
    if (session_) {
        VTDecompressionSessionInvalidate((VTDecompressionSessionRef)session_);
        CFRelease(session_);
        session_ = nullptr;
    }

    @autoreleasepool {
        // SDK 26.2 compiles @{} / @() literals to NSConstantDictionary / NSConstantIntegerNumber
        // which don't exist on macOS 15. Use CF APIs directly instead.
        int32_t pixFmt = kCVPixelFormatType_32BGRA;
        int32_t w32 = (int32_t)width_;
        int32_t h32 = (int32_t)height_;
        CFNumberRef pixFmtRef = CFNumberCreate(nullptr, kCFNumberSInt32Type, &pixFmt);
        CFNumberRef widthRef  = CFNumberCreate(nullptr, kCFNumberSInt32Type, &w32);
        CFNumberRef heightRef = CFNumberCreate(nullptr, kCFNumberSInt32Type, &h32);
        CFDictionaryRef ioSurfaceProps = CFDictionaryCreate(nullptr, nullptr, nullptr, 0,
            &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);

        const void* keys[] = {
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferWidthKey,
            kCVPixelBufferHeightKey,
            kCVPixelBufferIOSurfacePropertiesKey
        };
        const void* vals[] = { pixFmtRef, widthRef, heightRef, ioSurfaceProps };
        CFDictionaryRef attrs = CFDictionaryCreate(nullptr, keys, vals, 4,
            &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);

        VTDecompressionOutputCallbackRecord callback;
        callback.decompressionOutputCallback = decompress_callback;
        callback.decompressionOutputRefCon   = this;

        VTDecompressionSessionRef sess = nullptr;
        OSStatus status = VTDecompressionSessionCreate(
            kCFAllocatorDefault,
            (CMVideoFormatDescriptionRef)format_,
            nullptr, // videoDecoderSpecification
            attrs,
            &callback,
            &sess);

        CFRelease(pixFmtRef);
        CFRelease(widthRef);
        CFRelease(heightRef);
        CFRelease(ioSurfaceProps);
        CFRelease(attrs);

        if (status != noErr) {
            MELLO_LOG_ERROR(TAG, "VTDecompressionSessionCreate failed: %d", (int)status);
            return false;
        }

        // Enable low-latency mode
        VTSessionSetProperty(sess, kVTDecompressionPropertyKey_RealTime, kCFBooleanTrue);

        session_ = sess;
        MELLO_LOG_INFO(TAG, "VTDecompressionSession created: %ux%u BGRA output", width_, height_);
        return true;
    }
}

bool VTDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe) {
    auto nalus = parse_nalu_stream(data, size);
    if (nalus.empty()) return false;

    // On keyframe, extract SPS/PPS and (re)create the session
    if (is_keyframe) {
        const uint8_t* sps = nullptr; size_t sps_len = 0;
        const uint8_t* pps = nullptr; size_t pps_len = 0;

        for (auto& nalu : nalus) {
            if (nalu.type == 7) { sps = nalu.data; sps_len = nalu.size; }
            if (nalu.type == 8) { pps = nalu.data; pps_len = nalu.size; }
        }

        if (sps && pps) {
            if (!create_format_description(sps, sps_len, pps, pps_len)) return false;
            if (!create_session()) return false;
        }
    }

    if (!session_ || !format_) return false;

    // Feed each slice NAL to the decoder
    for (auto& nalu : nalus) {
        if (nalu.type == 7 || nalu.type == 8) continue; // skip parameter sets

        // Build AVCC-style sample: 4-byte big-endian length prefix + NAL data
        size_t avcc_size = 4 + nalu.size;
        std::vector<uint8_t> avcc(avcc_size);
        uint32_t len = (uint32_t)nalu.size;
        avcc[0] = (len >> 24) & 0xFF;
        avcc[1] = (len >> 16) & 0xFF;
        avcc[2] = (len >> 8)  & 0xFF;
        avcc[3] =  len        & 0xFF;
        memcpy(avcc.data() + 4, nalu.data, nalu.size);

        CMBlockBufferRef block = nullptr;
        OSStatus status = CMBlockBufferCreateWithMemoryBlock(
            kCFAllocatorDefault,
            avcc.data(), avcc_size,
            kCFAllocatorNull, // no dealloc — we own the buffer
            nullptr, 0, avcc_size,
            0, &block);

        if (status != noErr) continue;

        CMSampleBufferRef sample = nullptr;
        const size_t sample_size = avcc_size;
        status = CMSampleBufferCreate(
            kCFAllocatorDefault,
            block, true, nullptr, nullptr,
            (CMVideoFormatDescriptionRef)format_,
            1, 0, nullptr,
            1, &sample_size,
            &sample);

        CFRelease(block);
        if (status != noErr) continue;

        VTDecodeFrameFlags flags = kVTDecodeFrame_EnableAsynchronousDecompression
                                 | kVTDecodeFrame_1xRealTimePlayback;
        VTDecodeInfoFlags info_flags = 0;

        status = VTDecompressionSessionDecodeFrame(
            (VTDecompressionSessionRef)session_,
            sample, flags, nullptr, &info_flags);

        CFRelease(sample);

        if (status != noErr) {
            MELLO_LOG_WARN(TAG, "VTDecompressionSessionDecodeFrame failed: %d (nalu_type=%u)",
                (int)status, nalu.type);
        }
    }

    // Flush synchronously to ensure the callback fires before we return
    VTDecompressionSessionWaitForAsynchronousFrames((VTDecompressionSessionRef)session_);

    return true;
}

void* VTDecoder::get_frame_buffer() {
    std::lock_guard<std::mutex> lock(frame_mutex_);
    return latest_frame_; // Caller will retain if needed
}

bool VTDecoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264;
}

} // namespace mello::video

#endif
