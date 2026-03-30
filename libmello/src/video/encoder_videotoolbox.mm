#include "encoder_videotoolbox.hpp"

#ifdef __APPLE__

#include "../util/log.hpp"
#import <VideoToolbox/VideoToolbox.h>
#import <CoreVideo/CoreVideo.h>
#import <CoreMedia/CoreMedia.h>

namespace mello::video {

static constexpr const char* TAG = "video/encoder-vt";

// Convert AVCC (length-prefixed) NALUs to Annex B (start-code-prefixed) for wire format
static void avcc_to_annexb(CMSampleBufferRef sample, std::vector<uint8_t>& out, bool& is_keyframe) {
    out.clear();
    is_keyframe = false;

    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
    if (attachments && CFArrayGetCount(attachments) > 0) {
        CFDictionaryRef dict = (CFDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
        CFBooleanRef notSync = nullptr;
        if (CFDictionaryGetValueIfPresent(dict, kCMSampleAttachmentKey_NotSync, (const void**)&notSync)) {
            is_keyframe = (notSync == kCFBooleanFalse);
        } else {
            is_keyframe = true; // absence of NotSync means it IS a sync frame
        }
    }

    // If keyframe, prepend SPS and PPS from the format description
    if (is_keyframe) {
        CMFormatDescriptionRef fmt = CMSampleBufferGetFormatDescription(sample);
        size_t sps_size = 0, pps_size = 0;
        size_t param_count = 0;
        const uint8_t* sps = nullptr;
        const uint8_t* pps = nullptr;

        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, 0, &sps, &sps_size, &param_count, nullptr);
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, 1, &pps, &pps_size, nullptr, nullptr);

        if (sps && pps) {
            static const uint8_t start_code[] = {0, 0, 0, 1};
            out.insert(out.end(), start_code, start_code + 4);
            out.insert(out.end(), sps, sps + sps_size);
            out.insert(out.end(), start_code, start_code + 4);
            out.insert(out.end(), pps, pps + pps_size);
        }
    }

    CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
    if (!block) return;

    size_t total = 0;
    char*  data  = nullptr;
    CMBlockBufferGetDataPointer(block, 0, nullptr, &total, &data);
    if (!data) return;

    size_t offset = 0;
    while (offset < total) {
        // Read 4-byte AVCC length prefix (big-endian)
        if (offset + 4 > total) break;
        uint32_t nalu_len = ((uint32_t)(uint8_t)data[offset] << 24)
                          | ((uint32_t)(uint8_t)data[offset+1] << 16)
                          | ((uint32_t)(uint8_t)data[offset+2] << 8)
                          |  (uint32_t)(uint8_t)data[offset+3];
        offset += 4;

        if (offset + nalu_len > total) break;

        static const uint8_t start_code[] = {0, 0, 0, 1};
        out.insert(out.end(), start_code, start_code + 4);
        out.insert(out.end(), data + offset, data + offset + nalu_len);
        offset += nalu_len;
    }
}

// VTCompressionSession output callback
static void compress_callback(
    void* context,
    void* source_ref,
    OSStatus status,
    VTEncodeInfoFlags flags,
    CMSampleBufferRef sample)
{
    (void)source_ref; (void)flags;

    auto* self = static_cast<VTEncoder*>(context);
    if (status != noErr || !sample) {
        MELLO_LOG_WARN(TAG, "Encode callback: status=%d sample=%p", (int)status, sample);
        return;
    }

    std::vector<uint8_t> annexb;
    bool is_keyframe = false;
    avcc_to_annexb(sample, annexb, is_keyframe);

    std::lock_guard<std::mutex> lock(self->output_mutex_);
    self->output_data_         = std::move(annexb);
    self->output_is_keyframe_  = is_keyframe;
    self->output_ready_        = true;
}

VTEncoder::VTEncoder() = default;

VTEncoder::~VTEncoder() {
    shutdown();
}

bool VTEncoder::is_available() {
    return true; // Always available on Apple Silicon
}

bool VTEncoder::initialize(const GraphicsDevice& device, const EncoderConfig& config) {
    (void)device;
    width_   = config.width;
    height_  = config.height;
    fps_     = config.fps;
    bitrate_ = config.bitrate_kbps;
    keyframe_interval_ = config.keyframe_interval;

    @autoreleasepool {
        VTCompressionSessionRef sess = nullptr;

        OSStatus status = VTCompressionSessionCreate(
            kCFAllocatorDefault,
            (int32_t)width_, (int32_t)height_,
            kCMVideoCodecType_H264,
            nullptr, // encoderSpecification
            nullptr, // sourceImageBufferAttributes (accepts any CVPixelBuffer)
            kCFAllocatorDefault,
            compress_callback,
            this,
            &sess);

        if (status != noErr) {
            MELLO_LOG_ERROR(TAG, "VTCompressionSessionCreate failed: %d", (int)status);
            return false;
        }

        // Low-latency, real-time encoding
        VTSessionSetProperty(sess, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
        VTSessionSetProperty(sess, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);

        // Baseline profile — no B-frames, compatible with all decoders
        VTSessionSetProperty(sess, kVTCompressionPropertyKey_ProfileLevel,
            kVTProfileLevel_H264_Main_AutoLevel);

        // Bitrate: VBR with max = avg * 1.25 (matches spec §6.1)
        int avg_bps = (int)bitrate_ * 1000;
        CFNumberRef avg_ref = CFNumberCreate(nullptr, kCFNumberIntType, &avg_bps);
        VTSessionSetProperty(sess, kVTCompressionPropertyKey_AverageBitRate, avg_ref);
        CFRelease(avg_ref);

        // Data rate limits: [bytes_per_second, duration_in_seconds]
        int max_bytes_per_sec = avg_bps * 125 / 100 / 8; // 1.25x avg, in bytes
        double one_sec = 1.0;
        CFNumberRef limit_bytes = CFNumberCreate(nullptr, kCFNumberIntType, &max_bytes_per_sec);
        CFNumberRef limit_dur   = CFNumberCreate(nullptr, kCFNumberDoubleType, &one_sec);
        CFTypeRef limits[] = { limit_bytes, limit_dur };
        CFArrayRef limit_arr = CFArrayCreate(nullptr, limits, 2, &kCFTypeArrayCallBacks);
        VTSessionSetProperty(sess, kVTCompressionPropertyKey_DataRateLimits, limit_arr);
        CFRelease(limit_arr);
        CFRelease(limit_bytes);
        CFRelease(limit_dur);

        // Keyframe interval
        int kfi = (int)keyframe_interval_;
        CFNumberRef kfi_ref = CFNumberCreate(nullptr, kCFNumberIntType, &kfi);
        VTSessionSetProperty(sess, kVTCompressionPropertyKey_MaxKeyFrameInterval, kfi_ref);
        CFRelease(kfi_ref);

        // FPS hint
        float expected_fps = (float)fps_;
        CFNumberRef fps_ref = CFNumberCreate(nullptr, kCFNumberFloatType, &expected_fps);
        VTSessionSetProperty(sess, kVTCompressionPropertyKey_ExpectedFrameRate, fps_ref);
        CFRelease(fps_ref);

        VTCompressionSessionPrepareToEncodeFrames(sess);

        session_ = sess;
        memset(&stats_, 0, sizeof(stats_));

        MELLO_LOG_INFO(TAG, "VTEncoder initialized: %ux%u fps=%u bitrate=%ukbps keyframe_interval=%u",
            width_, height_, fps_, bitrate_, keyframe_interval_);
        return true;
    }
}

void VTEncoder::shutdown() {
    if (session_) {
        VTCompressionSessionCompleteFrames((VTCompressionSessionRef)session_, kCMTimeInvalid);
        VTCompressionSessionInvalidate((VTCompressionSessionRef)session_);
        CFRelease(session_);
        session_ = nullptr;
    }
    MELLO_LOG_INFO(TAG, "VTEncoder shutdown: frames=%llu keyframes=%u bytes=%lluMB",
        frame_count_, stats_.keyframes_sent, stats_.bytes_sent / (1024 * 1024));
}

bool VTEncoder::encode(void* cv_pixel_buffer, EncodedPacket& out) {
    if (!session_ || !cv_pixel_buffer) return false;

    CVPixelBufferRef pb = (CVPixelBufferRef)cv_pixel_buffer;

    CMTime pts = CMTimeMake(frame_count_, fps_);

    CFDictionaryRef frame_props = nullptr;
    if (force_keyframe_) {
        CFStringRef keys[] = { kVTEncodeFrameOptionKey_ForceKeyFrame };
        CFTypeRef   vals[] = { kCFBooleanTrue };
        frame_props = CFDictionaryCreate(nullptr,
            (const void**)keys, (const void**)vals, 1,
            &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        force_keyframe_ = false;
    }

    {
        std::lock_guard<std::mutex> lock(output_mutex_);
        output_ready_ = false;
    }

    OSStatus status = VTCompressionSessionEncodeFrame(
        (VTCompressionSessionRef)session_,
        pb, pts, kCMTimeInvalid,
        frame_props, nullptr, nullptr);

    if (frame_props) CFRelease(frame_props);

    if (status != noErr) {
        MELLO_LOG_WARN(TAG, "VTCompressionSessionEncodeFrame failed: %d (frame=%llu)",
            (int)status, frame_count_);
        return false;
    }

    // Force synchronous output for low-latency
    VTCompressionSessionCompleteFrames(
        (VTCompressionSessionRef)session_, kCMTimeInvalid);

    frame_count_++;

    std::lock_guard<std::mutex> lock(output_mutex_);
    if (!output_ready_ || output_data_.empty()) return false;

    out.data         = std::move(output_data_);
    out.is_keyframe  = output_is_keyframe_;
    out.timestamp_us = 0; // Filled by pipeline from capture timestamp

    stats_.bytes_sent += out.data.size();
    if (out.is_keyframe) stats_.keyframes_sent++;
    stats_.bitrate_kbps = bitrate_;
    stats_.fps_actual   = fps_; // TODO: compute actual from timestamps

    return true;
}

void VTEncoder::request_keyframe() {
    force_keyframe_ = true;
    MELLO_LOG_DEBUG(TAG, "Keyframe requested");
}

void VTEncoder::set_bitrate(uint32_t kbps) {
    if (!session_) return;
    bitrate_ = kbps;

    int avg_bps = (int)kbps * 1000;
    CFNumberRef avg_ref = CFNumberCreate(nullptr, kCFNumberIntType, &avg_bps);
    VTSessionSetProperty((VTCompressionSessionRef)session_,
        kVTCompressionPropertyKey_AverageBitRate, avg_ref);
    CFRelease(avg_ref);

    int max_bytes_per_sec = avg_bps * 125 / 100 / 8;
    double one_sec = 1.0;
    CFNumberRef limit_bytes = CFNumberCreate(nullptr, kCFNumberIntType, &max_bytes_per_sec);
    CFNumberRef limit_dur   = CFNumberCreate(nullptr, kCFNumberDoubleType, &one_sec);
    CFTypeRef limits[] = { limit_bytes, limit_dur };
    CFArrayRef limit_arr = CFArrayCreate(nullptr, limits, 2, &kCFTypeArrayCallBacks);
    VTSessionSetProperty((VTCompressionSessionRef)session_,
        kVTCompressionPropertyKey_DataRateLimits, limit_arr);
    CFRelease(limit_arr);
    CFRelease(limit_bytes);
    CFRelease(limit_dur);

    MELLO_LOG_INFO(TAG, "Bitrate updated: %u kbps", kbps);
}

void VTEncoder::get_stats(EncoderStats& out) const {
    out = stats_;
}

bool VTEncoder::supports_codec(VideoCodec codec) const {
    return codec == VideoCodec::H264;
}

} // namespace mello::video

#endif
