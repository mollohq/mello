// The game's sound on macOS (shared_game_audio.hpp): a Core Audio process tap
// (macOS 14.2) of the game's processes, read through a private aggregate
// device. Measured on 2026-11-04 (CLIPS.md 15.12a): the tap holds
// the game's sound at its full level and nothing of another app or of m3llo,
// where ScreenCaptureKit audio held the whole mix.
//
// A tap names processes, not a tree: a helper of the game plays into its own
// process only. The tap therefore holds the game's pid and each of its
// descendants that Core Audio knows (a process object exists once a process
// opens audio), and it is made again when that set changes.
//
// Threads: the IOProc runs on the capture's own serial queue and delivers to
// the consumers there. The process list listener runs on the same queue, so a
// new tap never races a delivery. Making and stopping the tap is on the
// caller's thread.
#include "shared_game_audio.hpp"
#include "stream_resampler.hpp"
#include "../util/log.hpp"

#import <CoreAudio/AudioHardware.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#import <Foundation/Foundation.h>

#include <libproc.h>
#include <mach/mach_time.h>
#include <sys/proc_info.h>

#include <algorithm>
#include <cmath>
#include <mutex>
#include <set>
#include <vector>

namespace mello::audio {

namespace {

constexpr const char* TAG = "game_audio";

/// The parent of `pid`, 0 when unknown.
pid_t parent_of(pid_t pid) {
    proc_bsdinfo info{};
    if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) != sizeof(info)) return 0;
    return static_cast<pid_t>(info.pbi_ppid);
}

bool in_tree(pid_t pid, pid_t root) {
    for (int depth = 0; pid > 1 && depth < 16; ++depth) {
        if (pid == root) return true;
        pid = parent_of(pid);
    }
    return false;
}

/// The Core Audio process objects of `root` and its descendants.
std::vector<AudioObjectID> process_objects(pid_t root) {
    AudioObjectPropertyAddress a{kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyScopeGlobal,
                                 kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    if (AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &a, 0, nullptr, &size) != noErr) return {};
    std::vector<AudioObjectID> all(size / sizeof(AudioObjectID));
    if (all.empty() || AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, 0, nullptr, &size, all.data()) != noErr) {
        return {};
    }
    std::vector<AudioObjectID> out;
    for (AudioObjectID object : all) {
        AudioObjectPropertyAddress pa{kAudioProcessPropertyPID, kAudioObjectPropertyScopeGlobal,
                                      kAudioObjectPropertyElementMain};
        pid_t pid = 0;
        UInt32 psize = sizeof(pid);
        if (AudioObjectGetPropertyData(object, &pa, 0, nullptr, &psize, &pid) != noErr) continue;
        if (in_tree(pid, root)) out.push_back(object);
    }
    std::sort(out.begin(), out.end());
    return out;
}

int64_t host_time_us(uint64_t host_time) {
    static const mach_timebase_info_data_t tb = [] {
        mach_timebase_info_data_t t;
        mach_timebase_info(&t);
        return t;
    }();
    // mach_absolute_time is libc++'s steady clock on Apple (mello_clock_now_us).
    return static_cast<int64_t>(host_time * tb.numer / tb.denom / 1000);
}

class MacGameAudioCapture final : public GameAudioCapture {
public:
    MacGameAudioCapture(pid_t pid, SharedGameAudio* shared) : pid_(pid), shared_(shared) {
        queue_ = dispatch_queue_create("mello.game_audio", DISPATCH_QUEUE_SERIAL);
    }

    ~MacGameAudioCapture() override {
        if (listening_) {
            AudioObjectPropertyAddress a{kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyScopeGlobal,
                                         kAudioObjectPropertyElementMain};
            AudioObjectRemovePropertyListenerBlock(kAudioObjectSystemObject, &a, queue_, listener_);
        }
        // The queue runs the IOProc and the listener: a stop on it waits for
        // a call in flight, so no delivery follows the destructor.
        dispatch_sync(queue_, ^{ close_tap(); });
    }

    bool start() {
        __block bool ok = false;
        dispatch_sync(queue_, ^{ ok = open_tap(); });
        // A game that opens its audio later (or a helper that starts later)
        // appears in the process list then.
        AudioObjectPropertyAddress a{kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyScopeGlobal,
                                     kAudioObjectPropertyElementMain};
        MacGameAudioCapture* self = this;
        listener_ = ^(UInt32, const AudioObjectPropertyAddress*) {
            if (process_objects(self->pid_) != self->objects_) {
                self->close_tap();
                self->open_tap();
            }
        };
        listening_ = AudioObjectAddPropertyListenerBlock(kAudioObjectSystemObject, &a, queue_, listener_) == noErr;
        return ok || listening_;
    }

private:
    // ---- The capture's queue ----

    bool open_tap() {
        objects_ = process_objects(pid_);
        if (objects_.empty()) {
            MELLO_LOG_INFO(TAG, "pid %d has no audio yet: the tap starts when it plays", pid_);
            return false;
        }
        NSMutableArray<NSNumber*>* list = [NSMutableArray array];
        for (AudioObjectID object : objects_) [list addObject:@(object)];
        CATapDescription* desc = [[CATapDescription alloc] initStereoMixdownOfProcesses:list];
        desc.privateTap = YES;
        desc.muteBehavior = CATapUnmuted;  // the user still hears the game
        OSStatus st = AudioHardwareCreateProcessTap(desc, &tap_);
        if (st != noErr) {
            MELLO_LOG_WARN(TAG, "AudioHardwareCreateProcessTap for pid %d failed: %d (audio capture permission?)",
                           pid_, static_cast<int>(st));
            tap_ = kAudioObjectUnknown;
            return false;
        }
        NSDictionary* aggregate = @{
            @kAudioAggregateDeviceNameKey : @"m3llo game audio",
            @kAudioAggregateDeviceUIDKey : [[NSUUID UUID] UUIDString],
            @kAudioAggregateDeviceIsPrivateKey : @YES,
            @kAudioAggregateDeviceTapAutoStartKey : @YES,
            @kAudioAggregateDeviceTapListKey : @[ @{@kAudioSubTapUIDKey : desc.UUID.UUIDString} ],
        };
        st = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef)aggregate, &device_);
        if (st != noErr) {
            MELLO_LOG_WARN(TAG, "the tap's aggregate device failed: %d", static_cast<int>(st));
            close_tap();
            return false;
        }
        AudioObjectPropertyAddress fa{kAudioTapPropertyFormat, kAudioObjectPropertyScopeGlobal,
                                      kAudioObjectPropertyElementMain};
        AudioStreamBasicDescription format{};
        UInt32 size = sizeof(format);
        AudioObjectGetPropertyData(tap_, &fa, 0, nullptr, &size, &format);
        if (format.mFormatID != kAudioFormatLinearPCM || !(format.mFormatFlags & kAudioFormatFlagIsFloat) ||
            format.mBitsPerChannel != 32 || format.mSampleRate <= 0) {
            MELLO_LOG_WARN(TAG, "the tap's format is not float PCM (id %u, %u bits)",
                           static_cast<unsigned>(format.mFormatID), static_cast<unsigned>(format.mBitsPerChannel));
            close_tap();
            return false;
        }
        rate_ = static_cast<int>(std::lround(format.mSampleRate));
        interleaved_ = !(format.mFormatFlags & kAudioFormatFlagIsNonInterleaved);
        resampler_.reset();
        if (rate_ != 48000) resampler_ = std::make_unique<StreamResampler>(rate_, 48000, 2);
        MacGameAudioCapture* self = this;
        st = AudioDeviceCreateIOProcIDWithBlock(&proc_, device_, queue_,
                                                ^(const AudioTimeStamp*, const AudioBufferList* in,
                                                  const AudioTimeStamp* in_time, AudioBufferList*,
                                                  const AudioTimeStamp*) { self->on_input(in, in_time); });
        if (st == noErr) st = AudioDeviceStart(device_, proc_);
        if (st != noErr) {
            MELLO_LOG_WARN(TAG, "the tap's device did not start: %d", static_cast<int>(st));
            close_tap();
            return false;
        }
        MELLO_LOG_INFO(TAG, "process tap of pid %d started: %zu process(es), %d Hz", pid_, objects_.size(), rate_);
        return true;
    }

    void close_tap() {
        if (proc_) {
            AudioDeviceStop(device_, proc_);
            AudioDeviceDestroyIOProcID(device_, proc_);
            proc_ = nullptr;
        }
        if (device_ != kAudioObjectUnknown) {
            AudioHardwareDestroyAggregateDevice(device_);
            device_ = kAudioObjectUnknown;
        }
        if (tap_ != kAudioObjectUnknown) {
            AudioHardwareDestroyProcessTap(tap_);
            tap_ = kAudioObjectUnknown;
        }
    }

    void on_input(const AudioBufferList* in, const AudioTimeStamp* in_time) {
        if (!in || in->mNumberBuffers == 0) return;
        const AudioBuffer& b0 = in->mBuffers[0];
        size_t frames = 0;
        stereo_.clear();
        if (interleaved_) {
            const size_t ch = std::max<UInt32>(b0.mNumberChannels, 1);
            frames = b0.mDataByteSize / (sizeof(float) * ch);
            const auto* d = static_cast<const float*>(b0.mData);
            for (size_t i = 0; i < frames; ++i) {
                stereo_.push_back(d[i * ch]);
                stereo_.push_back(ch > 1 ? d[i * ch + 1] : d[i * ch]);
            }
        } else {
            frames = b0.mDataByteSize / sizeof(float);
            const auto* l = static_cast<const float*>(b0.mData);
            const auto* r = in->mNumberBuffers > 1 ? static_cast<const float*>(in->mBuffers[1].mData) : l;
            for (size_t i = 0; i < frames; ++i) {
                stereo_.push_back(l[i]);
                stereo_.push_back(r[i]);
            }
        }
        if (frames == 0) return;
        int64_t capture_us = in_time && (in_time->mFlags & kAudioTimeStampHostTimeValid)
                                 ? host_time_us(in_time->mHostTime)
                                 : 0;
        if (resampler_) {
            // The resampler holds up to one input block (about 10 ms): its
            // output starts that much before this input.
            resampler_->push(stereo_.data(), frames);
            const size_t ready = resampler_->available();
            stereo_.resize(ready * 2);
            frames = resampler_->pull(stereo_.data(), ready);
            if (capture_us > 0) {
                capture_us -= static_cast<int64_t>(resampler_->in_block_frames()) * 1'000'000 / rate_;
            }
        }
        pcm_.resize(frames * 2);
        for (size_t i = 0; i < pcm_.size(); ++i) {
            const float v = std::clamp(stereo_[i], -1.0f, 1.0f);
            pcm_[i] = static_cast<int16_t>(std::lround(v * 32767.0f));
        }
        if (!pcm_.empty()) shared_->deliver(pcm_.data(), pcm_.size(), capture_us);
    }

    const pid_t pid_;
    SharedGameAudio* const shared_;
    dispatch_queue_t queue_;
    AudioObjectPropertyListenerBlock listener_ = nil;
    bool listening_ = false;

    // The capture's queue.
    std::vector<AudioObjectID> objects_;
    AudioObjectID tap_ = kAudioObjectUnknown;
    AudioObjectID device_ = kAudioObjectUnknown;
    AudioDeviceIOProcID proc_ = nullptr;
    int rate_ = 48000;
    bool interleaved_ = true;
    std::unique_ptr<StreamResampler> resampler_;
    std::vector<float> stereo_;
    std::vector<int16_t> pcm_;
};

}  // namespace

std::unique_ptr<GameAudioCapture> open_game_audio_capture(uint32_t pid, SharedGameAudio* shared) {
    auto capture = std::make_unique<MacGameAudioCapture>(static_cast<pid_t>(pid), shared);
    if (!capture->start()) {
        MELLO_LOG_WARN(TAG, "no process tap for pid %u", pid);
        return nullptr;
    }
    return capture;
}

}  // namespace mello::audio
