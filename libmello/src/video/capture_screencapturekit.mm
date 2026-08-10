#include "capture_screencapturekit.hpp"

#ifdef __APPLE__

#include "../util/log.hpp"
#include <vector>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <AppKit/AppKit.h>

namespace mello::video {

static constexpr const char* TAG = "video/capture-sck";

} // namespace mello::video

// Protocol conformance omitted: SDK 26.2 protocol metadata references classes
// that don't exist on macOS 15. Methods are still dispatched by selector at runtime.
@interface SCKDelegate : NSObject
@property (nonatomic, assign) mello::video::CaptureSource::FrameCallback videoCallback;
@property (nonatomic, assign) mello::video::SCKCapture* owner;
@property (nonatomic, assign) std::atomic<bool>* running;
@end

@implementation SCKDelegate {
    // Scratch for de-planarising audio. Only ever touched on the serial audio
    // sample queue, so it needs no lock.
    std::vector<float> _interleaved;
    bool               _loggedBadFormat;
}

- (void)stream:(SCStream*)stream didOutputSampleBuffer:(CMSampleBufferRef)buffer ofType:(SCStreamOutputType)type {
    if (!self.running || !self.running->load()) return;

    if (type == SCStreamOutputTypeScreen && self.videoCallback) {
        CVPixelBufferRef pixelBuffer = CMSampleBufferGetImageBuffer(buffer);
        if (!pixelBuffer) return;
        CMTime pts = CMSampleBufferGetPresentationTimeStamp(buffer);
        uint64_t ts_us = (uint64_t)(CMTimeGetSeconds(pts) * 1000000.0);
        self.videoCallback((void*)pixelBuffer, ts_us);
    } else if (type == SCStreamOutputTypeAudio && self.owner) {
        CMAudioFormatDescriptionRef fmtDesc =
            (CMAudioFormatDescriptionRef)CMSampleBufferGetFormatDescription(buffer);
        if (!fmtDesc) return;
        const AudioStreamBasicDescription* asbd =
            CMAudioFormatDescriptionGetStreamBasicDescription(fmtDesc);
        if (!asbd) return;

        // ScreenCaptureKit documents 32-bit float PCM. Reinterpreting anything
        // else as float would emit noise at full scale, so bail loudly instead.
        if (asbd->mFormatID != kAudioFormatLinearPCM ||
            !(asbd->mFormatFlags & kAudioFormatFlagIsFloat) ||
            asbd->mBitsPerChannel != 32) {
            if (!_loggedBadFormat) {
                _loggedBadFormat = true;
                MELLO_LOG_ERROR("video/capture-sck",
                    "unsupported game-audio format: id=%u flags=%u bits=%u — audio disabled",
                    (unsigned)asbd->mFormatID, (unsigned)asbd->mFormatFlags,
                    (unsigned)asbd->mBitsPerChannel);
            }
            return;
        }

        // SCK delivers non-interleaved (planar) float: one AudioBuffer per
        // channel. Reading the block buffer as if it were interleaved — which
        // is what a plain CMBlockBufferGetDataPointer invites — garbles it.
        // Ask for the AudioBufferList so both layouts are handled.
        constexpr size_t kMaxChannels = 16;
        uint8_t ablStorage[sizeof(AudioBufferList) + (kMaxChannels - 1) * sizeof(AudioBuffer)];
        AudioBufferList* abl = reinterpret_cast<AudioBufferList*>(ablStorage);
        CMBlockBufferRef retained = nullptr;

        OSStatus st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            buffer, nullptr, abl, sizeof(ablStorage), nullptr, nullptr, 0, &retained);
        if (st != noErr || !retained) return;

        const uint32_t sampleRate = (uint32_t)asbd->mSampleRate;
        const float*   out        = nullptr;
        uint32_t       channels   = 0;
        uint32_t       frameCount = 0;

        if (abl->mNumberBuffers == 1) {
            // Already interleaved.
            channels   = abl->mBuffers[0].mNumberChannels;
            if (channels > 0) {
                frameCount = abl->mBuffers[0].mDataByteSize / (sizeof(float) * channels);
                out        = (const float*)abl->mBuffers[0].mData;
            }
        } else {
            channels   = abl->mNumberBuffers;
            frameCount = abl->mBuffers[0].mDataByteSize / sizeof(float);
            _interleaved.resize((size_t)frameCount * channels);
            for (uint32_t ch = 0; ch < channels; ++ch) {
                const float* src = (const float*)abl->mBuffers[ch].mData;
                if (!src) { frameCount = 0; break; }
                for (uint32_t i = 0; i < frameCount; ++i) {
                    _interleaved[(size_t)i * channels + ch] = src[i];
                }
            }
            out = _interleaved.data();
        }

        if (out && channels > 0 && frameCount > 0) {
            self.owner->deliver_audio(out, frameCount, channels, sampleRate);
        }
        CFRelease(retained);
    }
}

- (void)stream:(SCStream*)stream didStopWithError:(NSError*)error {
    MELLO_LOG_WARN("video/capture-sck", "SCStream stopped: %s",
        [[error localizedDescription] UTF8String]);
    if (self.running) self.running->store(false);
}

@end

namespace mello::video {

SCKCapture::SCKCapture() = default;

SCKCapture::~SCKCapture() {
    stop();
    if (delegate_) {
        CFRelease(delegate_);
        delegate_ = nullptr;
    }
    if (filter_) {
        CFRelease(filter_);
        filter_ = nullptr;
    }
}

bool SCKCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    (void)device;

    // All ScreenCaptureKit / ObjC work MUST run on the main thread on macOS 15+.
    // Calling objc_msgSend for SCK classes from a tokio-rt-worker thread crashes.
    __block bool ok = false;
    __block uint32_t w = 0, h = 0;
    __block void* retained_filter = nullptr;

    CaptureMode mode = desc.mode;
    void* hwnd = desc.hwnd;
    uint32_t pid = desc.pid;
    uint32_t monitor_idx = desc.monitor_index;

    MELLO_LOG_INFO(TAG, "[init:1] mode=%d hwnd=%p pid=%u monitor=%u",
        (int)mode, hwnd, pid, monitor_idx);

    dispatch_sync(dispatch_get_main_queue(), ^{
        @autoreleasepool {
            __block SCShareableContent* content = nil;
            dispatch_semaphore_t sem = dispatch_semaphore_create(0);

            [SCShareableContent getShareableContentExcludingDesktopWindows:YES
                                                     onScreenWindowsOnly:NO
                                                        completionHandler:^(SCShareableContent* c, NSError* err) {
                if (err) {
                    MELLO_LOG_ERROR(TAG, "getShareableContent failed: %s",
                        [[err localizedDescription] UTF8String]);
                } else {
                    content = c;
                }
                dispatch_semaphore_signal(sem);
            }];

            dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));

            if (!content) {
                MELLO_LOG_ERROR(TAG, "Failed to get shareable content (screen recording permission?)");
                return;
            }

            SCContentFilter* filter = nil;

            switch (mode) {
                case CaptureMode::Monitor: {
                    NSArray<SCDisplay*>* displays = content.displays;
                    if (monitor_idx >= displays.count) {
                        MELLO_LOG_ERROR(TAG, "Monitor index %u out of range (have %lu)",
                            monitor_idx, (unsigned long)displays.count);
                        return;
                    }
                    SCDisplay* display = displays[monitor_idx];
                    w = (uint32_t)display.width;
                    h = (uint32_t)display.height;
                    filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:[NSArray array]];
                    MELLO_LOG_INFO(TAG, "Source: Monitor(%u) %ux%u", monitor_idx, w, h);
                    break;
                }
                case CaptureMode::Window: {
                    CGWindowID target_wid = (CGWindowID)(uintptr_t)hwnd;
                    MELLO_LOG_INFO(TAG, "[init:4] Window mode, wid=%u", target_wid);

                    SCWindow* target = nil;
                    for (SCWindow* win in content.windows) {
                        if (win.windowID == target_wid) {
                            target = win;
                            break;
                        }
                    }

                    if (!target) {
                        MELLO_LOG_WARN(TAG, "Window %u not in SCK list, falling back to primary display", target_wid);
                        if (content.displays.count == 0) return;
                        SCDisplay* display = content.displays[0];
                        w = (uint32_t)display.width;
                        h = (uint32_t)display.height;
                        filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:[NSArray array]];
                    } else {
                        w = (uint32_t)target.frame.size.width;
                        h = (uint32_t)target.frame.size.height;
                        if (w == 0 || h == 0) {
                            MELLO_LOG_ERROR(TAG, "Window %u has zero dimensions", target_wid);
                            return;
                        }
                        filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:target];
                        MELLO_LOG_INFO(TAG, "Source: Window(wid=%u) %ux%u", target_wid, w, h);
                    }
                    break;
                }
                case CaptureMode::Process: {
                    NSMutableArray<SCWindow*>* matching = [NSMutableArray array];
                    for (SCWindow* win in content.windows) {
                        if (win.owningApplication.processID == (pid_t)pid && win.isOnScreen) {
                            [matching addObject:win];
                        }
                    }
                    if (matching.count == 0) {
                        MELLO_LOG_ERROR(TAG, "No on-screen windows for pid=%u", pid);
                        return;
                    }
                    SCWindow* target = matching[0];
                    w = (uint32_t)target.frame.size.width;
                    h = (uint32_t)target.frame.size.height;
                    filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:target];
                    MELLO_LOG_INFO(TAG, "Source: Process(pid=%u \"%s\") window=%u %ux%u",
                        pid, [target.owningApplication.applicationName UTF8String],
                        (uint32_t)target.windowID, w, h);
                    break;
                }
            }

            if (!filter) return;
            retained_filter = (__bridge_retained void*)filter;
            MELLO_LOG_INFO(TAG, "[init:5] filter created %ux%u", w, h);
            ok = true;
        }
    });

    if (!ok) return false;

    width_  = w;
    height_ = h;
    filter_ = retained_filter;
    MELLO_LOG_INFO(TAG, "[init:6] initialize done %ux%u", width_, height_);
    return true;
}

void SCKCapture::set_audio_callback(AudioCallback cb) {
    std::lock_guard<std::mutex> lock(audio_mutex_);
    audio_callback_ = std::move(cb);
}

void SCKCapture::deliver_audio(const float* samples, uint32_t frame_count,
                               uint32_t channels, uint32_t sample_rate) {
    std::lock_guard<std::mutex> lock(audio_mutex_);
    if (audio_callback_) {
        audio_callback_(samples, frame_count, channels, sample_rate);
    }
}

bool SCKCapture::start(uint32_t target_fps, FrameCallback callback) {
    if (running_.load()) return false;
    callback_ = std::move(callback);

    __block bool ok = false;
    __block void* retained_stream = nullptr;
    __block void* retained_delegate = nullptr;

    uint32_t w = width_, h = height_;
    FrameCallback vcb = callback_;
    SCKCapture* self_ptr = this;
    std::atomic<bool>* running_ptr = &running_;
    void* filt = filter_;

    dispatch_sync(dispatch_get_main_queue(), ^{
        @autoreleasepool {
            MELLO_LOG_INFO(TAG, "[start:1] configuring %ux%u @ %u fps", w, h, target_fps);

            SCStreamConfiguration* config = [[SCStreamConfiguration alloc] init];
            config.width  = w;
            config.height = h;
            config.minimumFrameInterval = CMTimeMake(1, target_fps);
            config.pixelFormat = kCVPixelFormatType_32BGRA;
            config.showsCursor = NO;
            config.queueDepth  = 3;

            // Always on: SCStream fixes capturesAudio at creation, but
            // mello_stream_start_audio() runs after the host stream is already
            // up. Enabling it later is impossible, so enable it always and let
            // the audio callback decide whether the samples go anywhere.
            config.capturesAudio = YES;
            config.excludesCurrentProcessAudio = YES;
            config.channelCount = 2;
            config.sampleRate = 48000;

            MELLO_LOG_INFO(TAG, "[start:2] creating delegate + stream");
            SCKDelegate* del = [[SCKDelegate alloc] init];
            del.videoCallback = vcb;
            del.owner    = self_ptr;
            del.running  = running_ptr;
            retained_delegate = (__bridge_retained void*)del;

            SCContentFilter* filter = (__bridge SCContentFilter*)filt;
            NSError* err = nil;
            SCStream* stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:(id<SCStreamDelegate>)del];

            MELLO_LOG_INFO(TAG, "[start:3] adding stream outputs");
            dispatch_queue_t q = dispatch_queue_create("mello.capture", DISPATCH_QUEUE_SERIAL);
            dispatch_set_target_queue(q, dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0));

            [stream addStreamOutput:(id<SCStreamOutput>)del type:SCStreamOutputTypeScreen sampleHandlerQueue:q error:&err];
            if (err) {
                MELLO_LOG_ERROR(TAG, "addStreamOutput(screen) failed: %s", [[err localizedDescription] UTF8String]);
                return;
            }

            dispatch_queue_t aq = dispatch_queue_create("mello.capture.audio", DISPATCH_QUEUE_SERIAL);
            [stream addStreamOutput:(id<SCStreamOutput>)del type:SCStreamOutputTypeAudio sampleHandlerQueue:aq error:&err];
            if (err) {
                MELLO_LOG_WARN(TAG, "addStreamOutput(audio) failed: %s — continuing without game audio",
                    [[err localizedDescription] UTF8String]);
                err = nil;
            }

            MELLO_LOG_INFO(TAG, "[start:4] calling startCapture");
            dispatch_semaphore_t sem = dispatch_semaphore_create(0);
            __block bool started = false;

            [stream startCaptureWithCompletionHandler:^(NSError* startErr) {
                if (startErr) {
                    MELLO_LOG_ERROR(TAG, "startCapture failed: %s",
                        [[startErr localizedDescription] UTF8String]);
                } else {
                    started = true;
                }
                dispatch_semaphore_signal(sem);
            }];

            dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));

            if (!started) return;

            retained_stream = (__bridge_retained void*)stream;
            MELLO_LOG_INFO(TAG, "[start:5] SCStream started: %ux%u @ %u fps", w, h, target_fps);
            ok = true;
        }
    });

    if (!ok) return false;

    stream_   = retained_stream;
    delegate_ = retained_delegate;
    running_  = true;
    return true;
}

void SCKCapture::stop() {
    if (!running_.load()) return;
    running_ = false;

    if (stream_) {
        void* s = stream_;
        stream_ = nullptr;

        dispatch_sync(dispatch_get_main_queue(), ^{
            @autoreleasepool {
                SCStream* stream = (__bridge SCStream*)s;
                dispatch_semaphore_t sem = dispatch_semaphore_create(0);

                [stream stopCaptureWithCompletionHandler:^(NSError* err) {
                    if (err) {
                        MELLO_LOG_WARN(TAG, "stopCapture error: %s",
                            [[err localizedDescription] UTF8String]);
                    }
                    dispatch_semaphore_signal(sem);
                }];

                dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC));
                CFRelease(s);
            }
        });
    }

    MELLO_LOG_INFO(TAG, "SCStream stopped");
}

bool SCKCapture::get_cursor(CursorData& out) {
    __block int32_t cx = 0, cy = 0;

    dispatch_sync(dispatch_get_main_queue(), ^{
        @autoreleasepool {
            NSPoint pos = [NSEvent mouseLocation];
            NSScreen* main = [NSScreen mainScreen];
            CGFloat screen_h = main ? main.frame.size.height : 0;
            cx = (int32_t)pos.x;
            cy = (int32_t)(screen_h - pos.y);
        }
    });

    out.x = cx;
    out.y = cy;
    out.visible = true;
    out.shape_changed = false;
    return true;
}

// Factory function — creates the appropriate capture source for macOS
std::unique_ptr<CaptureSource> create_capture_source(const CaptureSourceDesc& desc) {
    return std::make_unique<SCKCapture>();
}

} // namespace mello::video

#endif
