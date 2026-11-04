// Reference clip capture on macOS (capture_reference.hpp): the capture-less
// source of the stream gate and of the game clip replay's tests. The same
// contract as the Windows source: frame n is due at start + n * 1e6 / fps on
// the steady clock, the callback carries the due time, every frame is
// delivered in order and the clip loops; the clip's audio comes in 10 ms
// chunks on the same clock, each handed over when complete.
//
// Upload. The thread reads frame n of the file into an NV12 pixel buffer
// (I420 clips interleave U and V), tagged BT.709 studio range, and a
// VTPixelTransferSession converts it to a BGRA buffer from a pool, as a
// ScreenCaptureKit frame is. A consumer may hold the BGRA buffer (retain):
// the pool makes a new one.
//
// Threads. initialize() and stop() run on the caller's thread. The source
// thread reads, converts and calls the frame callback; the audio thread calls
// the timed audio callback. info() is safe on any thread.
#include "capture_reference.hpp"

#if defined(__APPLE__) && !defined(MELLO_IOS)

#include "../util/log.hpp"

#import <CoreFoundation/CoreFoundation.h>
#import <CoreVideo/CoreVideo.h>
#import <VideoToolbox/VideoToolbox.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstring>
#include <fstream>
#include <mutex>
#include <thread>
#include <vector>

namespace mello::video {

namespace {

constexpr const char* TAG = "video/reference";

uint64_t steady_now_us() {
    return static_cast<uint64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(std::chrono::steady_clock::now().time_since_epoch())
            .count());
}

/// Sleep until `due_us`: to 1 ms before, then yield, for an exact wake.
void wait_until(uint64_t due_us, const std::atomic<bool>& running, bool spin) {
    for (;;) {
        const uint64_t now = steady_now_us();
        if (now >= due_us || !running.load(std::memory_order_relaxed)) return;
        const uint64_t left = due_us - now;
        if (left > 1500 || !spin) {
            std::this_thread::sleep_for(std::chrono::microseconds(spin ? left - 1000 : std::min<uint64_t>(left, 20'000)));
        } else {
            std::this_thread::yield();
        }
    }
}

}  // namespace

struct ReferenceCapture::Impl {
    ReferenceClip clip;
    uint64_t frames = 0;
    uint64_t frame_bytes = 0;
    std::ifstream file;
    std::vector<uint8_t> raw;  // one file frame
    CVPixelBufferRef nv12 = nullptr;
    CVPixelBufferPoolRef pool = nullptr;
    VTPixelTransferSessionRef transfer = nullptr;

    std::thread thread;
    std::atomic<bool> running{false};

    std::vector<float> audio;  // float stereo, one clip loop long
    std::thread audio_thread;
    std::mutex audio_cb_mutex;
    CaptureSource::TimedAudioCallback audio_cb;

    mutable std::mutex info_mutex;
    ReferenceInfo info{};

    ~Impl() {
        if (transfer) {
            VTPixelTransferSessionInvalidate(transfer);
            CFRelease(transfer);
        }
        if (pool) CVPixelBufferPoolRelease(pool);
        if (nv12) CVPixelBufferRelease(nv12);
    }

    bool read_frame(uint64_t index) {
        file.clear();
        file.seekg(static_cast<std::streamoff>(index * frame_bytes));
        file.read(reinterpret_cast<char*>(raw.data()), static_cast<std::streamsize>(frame_bytes));
        if (static_cast<uint64_t>(file.gcount()) != frame_bytes) return false;
        // Into the NV12 buffer, row by row (its rows may be padded).
        CVPixelBufferLockBaseAddress(nv12, 0);
        const size_t w = clip.width, h = clip.height;
        auto* y = static_cast<uint8_t*>(CVPixelBufferGetBaseAddressOfPlane(nv12, 0));
        auto* uv = static_cast<uint8_t*>(CVPixelBufferGetBaseAddressOfPlane(nv12, 1));
        const size_t y_stride = CVPixelBufferGetBytesPerRowOfPlane(nv12, 0);
        const size_t uv_stride = CVPixelBufferGetBytesPerRowOfPlane(nv12, 1);
        for (size_t r = 0; r < h; ++r) std::memcpy(y + r * y_stride, raw.data() + r * w, w);
        const uint8_t* chroma = raw.data() + w * h;
        for (size_t r = 0; r < h / 2; ++r) {
            uint8_t* row = uv + r * uv_stride;
            if (clip.nv12) {
                std::memcpy(row, chroma + r * w, w);
            } else {
                const uint8_t* u = chroma + r * (w / 2);
                const uint8_t* v = chroma + (w * h / 4) + r * (w / 2);
                for (size_t c = 0; c < w / 2; ++c) {
                    row[2 * c] = u[c];
                    row[2 * c + 1] = v[c];
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(nv12, 0);
        return true;
    }

    /// The current frame as a new BGRA buffer from the pool. Null on failure.
    CVPixelBufferRef convert() {
        CVPixelBufferRef out = nullptr;
        if (CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &out) != kCVReturnSuccess) return nullptr;
        if (VTPixelTransferSessionTransferImage(transfer, nv12, out) != noErr) {
            CVPixelBufferRelease(out);
            return nullptr;
        }
        return out;
    }
};

ReferenceCapture::ReferenceCapture() : impl_(std::make_unique<Impl>()) {}

ReferenceCapture::~ReferenceCapture() { stop(); }

bool ReferenceCapture::provides_timed_audio() const { return !impl_->audio.empty(); }

void ReferenceCapture::set_timed_audio_callback(TimedAudioCallback cb) {
    std::lock_guard<std::mutex> lock(impl_->audio_cb_mutex);
    impl_->audio_cb = std::move(cb);
}

uint32_t ReferenceCapture::width() const { return impl_->clip.width; }
uint32_t ReferenceCapture::height() const { return impl_->clip.height; }

ReferenceInfo ReferenceCapture::info() const {
    std::lock_guard<std::mutex> lock(impl_->info_mutex);
    return impl_->info;
}

bool ReferenceCapture::initialize(const GraphicsDevice&, const CaptureSourceDesc& desc) {
    if (desc.mode != CaptureMode::Reference) {
        MELLO_LOG_ERROR(TAG, "initialize: not a reference source");
        return false;
    }
    Impl& m = *impl_;
    m.clip = desc.reference;
    if (m.clip.fps == 0) m.clip.fps = 60;
    m.file.open(m.clip.path, std::ios::binary);
    if (!m.file) {
        MELLO_LOG_ERROR(TAG, "cannot open clip '%s'", m.clip.path.c_str());
        return false;
    }
    m.file.seekg(0, std::ios::end);
    const auto size = static_cast<uint64_t>(m.file.tellg());
    m.frames = reference_clip_frames(size, m.clip.width, m.clip.height);
    if (m.frames == 0) {
        MELLO_LOG_ERROR(TAG, "clip '%s' (%llu bytes) is not a whole number of %ux%u 4:2:0 frames", m.clip.path.c_str(),
                        static_cast<unsigned long long>(size), m.clip.width, m.clip.height);
        return false;
    }
    m.frame_bytes = reference_frame_bytes(m.clip.width, m.clip.height);
    m.raw.resize(m.frame_bytes);

    // The clip's audio, one loop long at 48 kHz stereo.
    m.audio.clear();
    if (!m.clip.audio_path.empty()) {
        std::ifstream af(m.clip.audio_path, std::ios::binary);
        if (!af) {
            MELLO_LOG_ERROR(TAG, "cannot open clip audio '%s'", m.clip.audio_path.c_str());
            return false;
        }
        const uint64_t loop_frames = m.frames * 48'000ULL / m.clip.fps;
        std::vector<int16_t> pcm(static_cast<size_t>(loop_frames * 2), 0);
        af.read(reinterpret_cast<char*>(pcm.data()), static_cast<std::streamsize>(pcm.size() * 2));
        const auto got = static_cast<uint64_t>(af.gcount());
        m.audio.resize(pcm.size());
        for (size_t i = 0; i < pcm.size(); ++i) m.audio[i] = static_cast<float>(pcm[i]) / 32768.0f;
        MELLO_LOG_INFO(TAG, "clip audio '%s': %llu of %llu frames (48 kHz stereo)", m.clip.audio_path.c_str(),
                       static_cast<unsigned long long>(got / 4), static_cast<unsigned long long>(loop_frames));
    }

    // The NV12 source buffer, BT.709 studio range, and the BGRA pool.
    NSDictionary* surface = @{(id)kCVPixelBufferIOSurfacePropertiesKey : @{}};
    if (CVPixelBufferCreate(kCFAllocatorDefault, m.clip.width, m.clip.height,
                            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, (__bridge CFDictionaryRef)surface,
                            &m.nv12) != kCVReturnSuccess) {
        MELLO_LOG_ERROR(TAG, "no NV12 buffer");
        return false;
    }
    CVBufferSetAttachment(m.nv12, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                          kCVAttachmentMode_ShouldPropagate);
    CVBufferSetAttachment(m.nv12, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2,
                          kCVAttachmentMode_ShouldPropagate);
    CVBufferSetAttachment(m.nv12, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2,
                          kCVAttachmentMode_ShouldPropagate);
    NSDictionary* bgra = @{
        (id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_32BGRA),
        (id)kCVPixelBufferWidthKey : @(m.clip.width),
        (id)kCVPixelBufferHeightKey : @(m.clip.height),
        (id)kCVPixelBufferIOSurfacePropertiesKey : @{},
    };
    if (CVPixelBufferPoolCreate(kCFAllocatorDefault, nullptr, (__bridge CFDictionaryRef)bgra, &m.pool) !=
            kCVReturnSuccess ||
        VTPixelTransferSessionCreate(kCFAllocatorDefault, &m.transfer) != noErr) {
        MELLO_LOG_ERROR(TAG, "no BGRA pool or pixel transfer");
        return false;
    }
    {
        std::lock_guard<std::mutex> lock(m.info_mutex);
        m.info = {};
        m.info.clip_frames = static_cast<uint32_t>(m.frames);
        m.info.fps = m.clip.fps;
    }
    MELLO_LOG_INFO(TAG, "clip '%s': %ux%u %s, %llu frames at %u fps", m.clip.path.c_str(), m.clip.width, m.clip.height,
                   m.clip.nv12 ? "NV12" : "I420", static_cast<unsigned long long>(m.frames), m.clip.fps);
    return true;
}

bool ReferenceCapture::start(uint32_t target_fps, FrameCallback callback) {
    Impl& m = *impl_;
    if (m.running.load() || !callback || !m.transfer) return false;
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
        // A little room for the first conversion, then a fixed schedule.
        const uint64_t start = steady_now_us() + 20'000;
        {
            std::lock_guard<std::mutex> lock(s.info_mutex);
            s.info.start_us = start;
        }
        if (!s.audio.empty() && !s.audio_thread.joinable()) {
            // The clip's audio on the same clock, frame 0 at start.
            s.audio_thread = std::thread([&s, start]() {
                constexpr uint32_t kChunk = 480;  // 10 ms
                const size_t loop = s.audio.size() / 2;
                std::vector<float> chunk(kChunk * 2);
                uint64_t j = 0;
                while (s.running.load(std::memory_order_relaxed)) {
                    const uint64_t captured = start + j * 10'000;
                    // Handed over when complete, as a capture device does.
                    wait_until(captured + 10'000, s.running, false);
                    if (!s.running.load(std::memory_order_relaxed)) break;
                    for (uint32_t i = 0; i < kChunk; ++i) {
                        const size_t f = static_cast<size_t>((j * kChunk + i) % loop);
                        chunk[i * 2] = s.audio[f * 2];
                        chunk[i * 2 + 1] = s.audio[f * 2 + 1];
                    }
                    {
                        std::lock_guard<std::mutex> lock(s.audio_cb_mutex);
                        if (s.audio_cb) s.audio_cb(chunk.data(), kChunk, 2, 48'000, static_cast<int64_t>(captured));
                    }
                    ++j;
                }
            });
        }
        uint64_t n = 0;
        while (s.running.load(std::memory_order_relaxed)) {
            const uint64_t due = reference_due_us(start, n, s.clip.fps);
            wait_until(due, s.running, true);
            if (!s.running.load(std::memory_order_relaxed)) break;
            CVPixelBufferRef frame = s.convert();
            if (!frame) {
                MELLO_LOG_ERROR(TAG, "pixel transfer failed; source stops");
                failed_.store(true, std::memory_order_relaxed);
                break;
            }
            const uint64_t handed = steady_now_us();
            callback(frame, due);
            CVPixelBufferRelease(frame);  // a consumer that keeps it retained it
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
                MELLO_LOG_ERROR(TAG, "cannot read frame %llu; source stops", static_cast<unsigned long long>(n % s.frames));
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
    if (m.thread.joinable()) m.thread.join();
    if (m.audio_thread.joinable()) m.audio_thread.join();
}

}  // namespace mello::video

#endif
