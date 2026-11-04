// Game clips on macOS (CLIPS.md 15.12a): which capture gives
// the game's sound only. Not a test; ctest does not run it.
//
// It records three candidates at once for a few seconds and reports, for
// each, the level of four tones and the clock of its samples:
//   A  ScreenCaptureKit audio on the game window's own filter
//      (initWithDesktopIndependentWindow), as the replay's capture has it
//   B  ScreenCaptureKit audio on a display filter that includes only the
//      game's application
//   C  a Core Audio process tap of the game's process (macOS 14.2)
//
// Roles of the one binary (`--role`):
//   probe      the recorder. Plays 880 Hz itself: m3llo's own sound, which
//              no candidate may hold (excludesCurrentProcessAudio).
//   fake-game  a window and 440 Hz. It starts a child (fake-sound 1000 Hz):
//              a helper process of the game.
//   fake-sound no window, a tone (`--hz`): the child, or another app (660 Hz).
//
//   mello_sck_audio_probe --role probe --self-test --seconds 8
//      [--game-exe FakeGame.app/Contents/MacOS/fakegame]
//      starts the fake game and the other app, records, prints JSON. With
//      `--game-exe` the fake game runs from its own app bundle, as a game
//      does (ScreenCaptureKit groups the sound by application).
//   mello_sck_audio_probe --role probe --pid <game pid> --seconds 600
//      records a real game (the levels of the four tones then say only
//      that m3llo and the test tones stay out; the game's own level is
//      the RMS).

#import <AppKit/AppKit.h>
#import <AudioToolbox/AudioToolbox.h>
#import <CoreAudio/AudioHardware.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#import <CoreMedia/CoreMedia.h>
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

#include <mach/mach_time.h>
#include <signal.h>
#include <spawn.h>
#include <unistd.h>

#include <atomic>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <string>
#include <vector>

extern char** environ;

namespace {

// ---- Tones ------------------------------------------------------------------

struct Tone {
    double hz = 440;
    double phase = 0;
};

OSStatus render_tone(void* user, AudioUnitRenderActionFlags*, const AudioTimeStamp*, UInt32, UInt32 frames,
                     AudioBufferList* io) {
    auto* tone = static_cast<Tone*>(user);
    auto* l = static_cast<float*>(io->mBuffers[0].mData);
    auto* r = io->mNumberBuffers > 1 ? static_cast<float*>(io->mBuffers[1].mData) : nullptr;
    for (UInt32 i = 0; i < frames; ++i) {
        const float v = static_cast<float>(0.25 * std::sin(tone->phase));
        tone->phase += 2 * M_PI * tone->hz / 48000.0;
        if (tone->phase > 2 * M_PI) tone->phase -= 2 * M_PI;
        l[i] = v;
        if (r) r[i] = v;
    }
    return noErr;
}

/// Plays a tone on the default output until the process ends.
AudioUnit play_tone(Tone* tone) {
    AudioComponentDescription d{kAudioUnitType_Output, kAudioUnitSubType_DefaultOutput, kAudioUnitManufacturer_Apple, 0, 0};
    AudioComponent c = AudioComponentFindNext(nullptr, &d);
    AudioUnit unit = nullptr;
    if (!c || AudioComponentInstanceNew(c, &unit) != noErr) return nullptr;
    AudioStreamBasicDescription f{};
    f.mSampleRate = 48000;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved;
    f.mChannelsPerFrame = 2;
    f.mFramesPerPacket = 1;
    f.mBitsPerChannel = 32;
    f.mBytesPerFrame = 4;
    f.mBytesPerPacket = 4;
    AudioUnitSetProperty(unit, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0, &f, sizeof(f));
    AURenderCallbackStruct cb{render_tone, tone};
    AudioUnitSetProperty(unit, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &cb, sizeof(cb));
    if (AudioUnitInitialize(unit) != noErr || AudioOutputUnitStart(unit) != noErr) return nullptr;
    return unit;
}

pid_t spawn_self(const char* self, std::vector<std::string> args) {
    std::vector<char*> argv;
    argv.push_back(const_cast<char*>(self));
    for (auto& a : args) argv.push_back(a.data());
    argv.push_back(nullptr);
    pid_t pid = 0;
    if (posix_spawn(&pid, self, nullptr, nullptr, argv.data(), environ) != 0) return 0;
    return pid;
}

// ---- Analysis ---------------------------------------------------------------

uint64_t host_now_ns() {
    static mach_timebase_info_data_t tb = [] {
        mach_timebase_info_data_t t;
        mach_timebase_info(&t);
        return t;
    }();
    return mach_absolute_time() * tb.numer / tb.denom;
}

/// One candidate's samples (stereo float, interleaved) and its clock.
struct Recording {
    std::string name;
    std::string status = "not started";
    std::mutex mutex;
    std::vector<float> pcm;
    double rate = 48000;
    std::atomic<uint64_t> callbacks{0};
    uint64_t buffers = 0;
    double pts_offset_ms_sum = 0;  // delivery time minus the sample time
    double pts_offset_ms_min = 1e9;
    double pts_offset_ms_max = -1e9;

    void add(const float* l, const float* r, size_t frames, size_t stride, double sample_host_s) {
        std::lock_guard<std::mutex> lock(mutex);
        for (size_t i = 0; i < frames; ++i) {
            pcm.push_back(l[i * stride]);
            pcm.push_back(r ? r[i * stride] : l[i * stride]);
        }
        ++buffers;
        if (sample_host_s > 0) {
            const double off = static_cast<double>(host_now_ns()) / 1e6 - sample_host_s * 1e3;
            pts_offset_ms_sum += off;
            pts_offset_ms_min = std::min(pts_offset_ms_min, off);
            pts_offset_ms_max = std::max(pts_offset_ms_max, off);
        }
    }
};

double goertzel(const std::vector<float>& s, int ch, double hz, double rate) {
    const double k = 2 * std::cos(2 * M_PI * hz / rate);
    double s1 = 0, s2 = 0;
    const size_t n = s.size() / 2;
    for (size_t i = 0; i < n; ++i) {
        const double v = s[2 * i + ch] + k * s1 - s2;
        s2 = s1;
        s1 = v;
    }
    return n ? 2 * std::sqrt(std::max(0.0, s1 * s1 + s2 * s2 - k * s1 * s2)) / static_cast<double>(n) : 0;
}

void report(Recording& r) {
    std::lock_guard<std::mutex> lock(r.mutex);
    const size_t n = r.pcm.size() / 2;
    double rms = 0;
    for (float v : r.pcm) rms += v * v;
    rms = r.pcm.empty() ? 0 : std::sqrt(rms / r.pcm.size());
    std::printf("  \"%s\": {\"status\": \"%s\", \"seconds\": %.2f, \"rate\": %.0f, \"buffers\": %llu, \"callbacks\": %llu, \"rms\": %.4f, ",
                r.name.c_str(), r.status.c_str(), n / r.rate, r.rate, static_cast<unsigned long long>(r.buffers),
                static_cast<unsigned long long>(r.callbacks.load()), rms);
    std::printf("\"tone_game_440\": %.4f, \"tone_child_1000\": %.4f, \"tone_other_app_660\": %.4f, \"tone_m3llo_880\": %.4f",
                goertzel(r.pcm, 0, 440, r.rate), goertzel(r.pcm, 0, 1000, r.rate), goertzel(r.pcm, 0, 660, r.rate),
                goertzel(r.pcm, 0, 880, r.rate));
    if (r.buffers > 0 && r.pts_offset_ms_min < 1e8) {
        std::printf(", \"delivery_minus_sample_ms\": {\"mean\": %.2f, \"min\": %.2f, \"max\": %.2f}",
                    r.pts_offset_ms_sum / r.buffers, r.pts_offset_ms_min, r.pts_offset_ms_max);
    }
    std::printf("}");
}

}  // namespace

// ---- ScreenCaptureKit -------------------------------------------------------

@interface ProbeOutput : NSObject <SCStreamOutput, SCStreamDelegate>
@property(nonatomic) Recording* recording;
@end

@implementation ProbeOutput
- (void)stream:(SCStream*)stream didOutputSampleBuffer:(CMSampleBufferRef)buffer ofType:(SCStreamOutputType)type {
    if (type != SCStreamOutputTypeAudio) return;
    const auto* asbd = CMAudioFormatDescriptionGetStreamBasicDescription(
        (CMAudioFormatDescriptionRef)CMSampleBufferGetFormatDescription(buffer));
    if (!asbd || !(asbd->mFormatFlags & kAudioFormatFlagIsFloat)) return;
    self.recording->rate = asbd->mSampleRate;
    self.recording->callbacks++;
    // Planar stereo: room for two buffers in the list.
    struct {
        AudioBufferList list;
        AudioBuffer second;
    } storage{};
    AudioBufferList& list = storage.list;
    CMBlockBufferRef block = nullptr;
    const OSStatus st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        buffer, nullptr, &list, sizeof(storage), nullptr, nullptr, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
        &block);
    if (st != noErr) {
        self.recording->status = "audio buffer list: " + std::to_string(st);
        return;
    }
    const size_t frames = CMSampleBufferGetNumSamples(buffer);
    const double pts = CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(buffer));
    if (list.mNumberBuffers >= 2) {
        self.recording->add(static_cast<float*>(list.mBuffers[0].mData), static_cast<float*>(list.mBuffers[1].mData), frames,
                            1, pts);
    } else if (list.mNumberBuffers == 1) {
        const size_t ch = list.mBuffers[0].mNumberChannels;
        const float* d = static_cast<float*>(list.mBuffers[0].mData);
        self.recording->add(d, ch > 1 ? d + 1 : nullptr, frames, ch, pts);
    }
    if (block) CFRelease(block);
}
- (void)stream:(SCStream*)stream didStopWithError:(NSError*)error {
    self.recording->status = std::string("stopped: ") + error.localizedDescription.UTF8String;
}
@end

static SCStream* start_sck(SCContentFilter* filter, Recording* rec, ProbeOutput* out, uint32_t w, uint32_t h) {
    SCStreamConfiguration* config = [[SCStreamConfiguration alloc] init];
    config.width = w;
    config.height = h;
    config.minimumFrameInterval = CMTimeMake(1, 1);
    config.capturesAudio = YES;
    config.excludesCurrentProcessAudio = YES;
    config.sampleRate = 48000;
    config.channelCount = 2;
    out.recording = rec;
    SCStream* stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:out];
    NSError* err = nil;
    dispatch_queue_t q = dispatch_queue_create("probe.audio", DISPATCH_QUEUE_SERIAL);
    [stream addStreamOutput:out type:SCStreamOutputTypeAudio sampleHandlerQueue:q error:&err];
    // A stream with no screen output logs an error for each frame; take them.
    [stream addStreamOutput:out type:SCStreamOutputTypeScreen sampleHandlerQueue:q error:&err];
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block bool ok = false;
    [stream startCaptureWithCompletionHandler:^(NSError* e) {
        ok = e == nil;
        if (e) rec->status = std::string("start failed: ") + e.localizedDescription.UTF8String;
        dispatch_semaphore_signal(sem);
    }];
    dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
    if (ok) rec->status = "recording";
    return ok ? stream : nil;
}

// ---- Core Audio process tap -------------------------------------------------

static bool start_tap(pid_t pid, Recording* rec, AudioObjectID* out_tap, AudioObjectID* out_agg, AudioDeviceIOProcID* out_proc) {
    AudioObjectPropertyAddress a{kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyScopeGlobal,
                                 kAudioObjectPropertyElementMain};
    AudioObjectID process = kAudioObjectUnknown;
    UInt32 size = sizeof(process);
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, sizeof(pid), &pid, &size, &process) != noErr ||
        process == kAudioObjectUnknown) {
        rec->status = "no audio process object for the pid (it has not played sound yet?)";
        return false;
    }
    CATapDescription* desc = [[CATapDescription alloc] initStereoMixdownOfProcesses:@[ @(process) ]];
    desc.privateTap = YES;
    desc.muteBehavior = CATapUnmuted;
    AudioObjectID tap = kAudioObjectUnknown;
    OSStatus st = AudioHardwareCreateProcessTap(desc, &tap);
    if (st != noErr) {
        rec->status = "AudioHardwareCreateProcessTap failed: " + std::to_string(st);
        return false;
    }
    NSDictionary* agg = @{
        @kAudioAggregateDeviceNameKey : @"mello probe tap",
        @kAudioAggregateDeviceUIDKey : [[NSUUID UUID] UUIDString],
        @kAudioAggregateDeviceIsPrivateKey : @YES,
        @kAudioAggregateDeviceTapAutoStartKey : @YES,
        @kAudioAggregateDeviceTapListKey : @[ @{@kAudioSubTapUIDKey : desc.UUID.UUIDString} ],
    };
    AudioObjectID device = kAudioObjectUnknown;
    st = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef)agg, &device);
    if (st != noErr) {
        rec->status = "AudioHardwareCreateAggregateDevice failed: " + std::to_string(st);
        AudioHardwareDestroyProcessTap(tap);
        return false;
    }
    AudioObjectPropertyAddress fa{kAudioTapPropertyFormat, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioStreamBasicDescription fmt{};
    size = sizeof(fmt);
    AudioObjectGetPropertyData(tap, &fa, 0, nullptr, &size, &fmt);
    rec->rate = fmt.mSampleRate > 0 ? fmt.mSampleRate : 48000;
    const bool interleaved = !(fmt.mFormatFlags & kAudioFormatFlagIsNonInterleaved);
    AudioDeviceIOProcID proc = nullptr;
    dispatch_queue_t q = dispatch_queue_create("probe.tap", DISPATCH_QUEUE_SERIAL);
    st = AudioDeviceCreateIOProcIDWithBlock(&proc, device, q,
                                            ^(const AudioTimeStamp*, const AudioBufferList* in, const AudioTimeStamp* in_time,
                                              AudioBufferList*, const AudioTimeStamp*) {
                                                if (!in || in->mNumberBuffers == 0) return;
                                                static mach_timebase_info_data_t tb = [] {
                                                    mach_timebase_info_data_t t;
                                                    mach_timebase_info(&t);
                                                    return t;
                                                }();
                                                const double host_s = in_time && (in_time->mFlags & kAudioTimeStampHostTimeValid)
                                                                          ? static_cast<double>(in_time->mHostTime) * tb.numer / tb.denom / 1e9
                                                                          : 0;
                                                const AudioBuffer& b0 = in->mBuffers[0];
                                                const float* d = static_cast<const float*>(b0.mData);
                                                if (interleaved) {
                                                    const size_t ch = b0.mNumberChannels;
                                                    rec->add(d, ch > 1 ? d + 1 : nullptr, b0.mDataByteSize / (4 * ch), ch, host_s);
                                                } else {
                                                    const float* r = in->mNumberBuffers > 1 ? static_cast<const float*>(in->mBuffers[1].mData) : nullptr;
                                                    rec->add(d, r, b0.mDataByteSize / 4, 1, host_s);
                                                }
                                            });
    if (st != noErr || AudioDeviceStart(device, proc) != noErr) {
        rec->status = "the tap's device did not start: " + std::to_string(st);
        return false;
    }
    rec->status = "recording";
    *out_tap = tap;
    *out_agg = device;
    *out_proc = proc;
    return true;
}

// ---- Roles ------------------------------------------------------------------

static int fake_sound(double hz) {
    static Tone tone;
    tone.hz = hz;
    if (!play_tone(&tone)) return 1;
    for (;;) pause();
}

static int fake_game(const char* self, double hz) {
    @autoreleasepool {
        NSApplication* app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        NSWindow* window = [[NSWindow alloc] initWithContentRect:NSMakeRect(200, 200, 640, 360)
                                                       styleMask:NSWindowStyleMaskTitled
                                                         backing:NSBackingStoreBuffered
                                                           defer:NO];
        window.title = @"mello fake game";
        window.backgroundColor = [NSColor colorWithCalibratedRed:0.2 green:0.3 blue:0.5 alpha:1];
        [window makeKeyAndOrderFront:nil];
        [app activateIgnoringOtherApps:YES];
        static Tone tone;
        tone.hz = hz;
        play_tone(&tone);
        // A helper process of the game with its own sound.
        static pid_t child = spawn_self(self, {"--role", "fake-sound", "--hz", "1000"});
        signal(SIGTERM, SIG_IGN);
        static dispatch_source_t term =
            dispatch_source_create(DISPATCH_SOURCE_TYPE_SIGNAL, SIGTERM, 0, dispatch_get_main_queue());
        dispatch_source_set_event_handler(term, ^{
            if (child > 0) kill(child, SIGTERM);
            _exit(0);
        });
        dispatch_resume(term);
        [app run];
    }
    return 0;
}

static NSRunningApplication* wait_for_app(pid_t pid) {
    for (int i = 0; i < 100; ++i) {
        NSRunningApplication* a = [NSRunningApplication runningApplicationWithProcessIdentifier:pid];
        if (a) return a;
        [NSThread sleepForTimeInterval:0.05];
    }
    return nil;
}

static pid_t g_game = 0;
static pid_t g_other = 0;

static void stop_fakes() {
    // The game's helper is the game's child: the game takes it down.
    if (g_game > 0) kill(g_game, SIGTERM);
    if (g_other > 0) kill(g_other, SIGTERM);
}

static int probe(const char* self, const char* game_exe, pid_t pid, double seconds, bool self_test) {
    // ScreenCaptureKit needs the CoreGraphics session of an application.
    [NSApplication sharedApplication];
    pid_t game = pid;
    if (self_test) {
        // The fake game from its own app bundle when `--game-exe` names one:
        // ScreenCaptureKit groups the sound by application.
        g_game = game = spawn_self(game_exe ? game_exe : self, {"--role", "fake-game", "--hz", "440"});
        g_other = spawn_self(self, {"--role", "fake-sound", "--hz", "660"});
        atexit(stop_fakes);
        signal(SIGABRT, [](int) {
            stop_fakes();
            _exit(134);
        });
        [[NSRunLoop currentRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:2.0]];
    }
    static Tone own;
    own.hz = 880;
    play_tone(&own);

    // The game's window and application in ScreenCaptureKit.
    __block SCShareableContent* content = nil;
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    [SCShareableContent getShareableContentExcludingDesktopWindows:YES
                                               onScreenWindowsOnly:NO
                                                 completionHandler:^(SCShareableContent* c, NSError*) {
                                                     content = c;
                                                     dispatch_semaphore_signal(sem);
                                                 }];
    dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
    SCWindow* window = nil;
    for (SCWindow* w in content.windows) {
        if (w.owningApplication.processID == game && w.windowLayer == 0 && w.frame.size.width > 100 &&
            (!window || w.frame.size.width * w.frame.size.height > window.frame.size.width * window.frame.size.height)) {
            window = w;
        }
    }
    SCRunningApplication* app = nil;
    for (SCRunningApplication* a in content.applications) {
        if (a.processID == game) app = a;
    }

    Recording a, b, c;
    a.name = "A_sck_window_filter";
    b.name = "B_sck_display_including_app";
    c.name = "C_core_audio_process_tap";
    ProbeOutput* out_a = [ProbeOutput new];
    ProbeOutput* out_b = [ProbeOutput new];
    SCStream* sa = nil;
    SCStream* sb = nil;
    if (window) {
        sa = start_sck([[SCContentFilter alloc] initWithDesktopIndependentWindow:window], &a, out_a, 64, 36);
    } else {
        a.status = "no window of the pid";
    }
    if (app && content.displays.count > 0) {
        sb = start_sck([[SCContentFilter alloc] initWithDisplay:content.displays[0]
                                          includingApplications:@[ app ]
                                               exceptingWindows:@[]],
                       &b, out_b, 2, 2);
    } else {
        b.status = "no application of the pid";
    }
    AudioObjectID tap = 0, agg = 0;
    AudioDeviceIOProcID proc = nullptr;
    start_tap(game, &c, &tap, &agg, &proc);

    // The main run loop runs while the captures record: ScreenCaptureKit
    // delivers nothing to a process whose main thread sleeps.
    [[NSRunLoop currentRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:seconds]];

    if (sa) [sa stopCaptureWithCompletionHandler:^(NSError*){}];
    if (sb) [sb stopCaptureWithCompletionHandler:^(NSError*){}];
    if (proc) {
        AudioDeviceStop(agg, proc);
        AudioDeviceDestroyIOProcID(agg, proc);
    }
    if (agg) AudioHardwareDestroyAggregateDevice(agg);
    if (tap) AudioHardwareDestroyProcessTap(tap);
    stop_fakes();

    std::printf("{\n  \"game_pid\": %d, \"window\": %s, \"seconds\": %.1f,\n", game, window ? "true" : "false", seconds);
    report(a);
    std::printf(",\n");
    report(b);
    std::printf(",\n");
    report(c);
    std::printf("\n}\n");
    return 0;
}

int main(int argc, char** argv) {
    std::string role = "probe";
    double hz = 440;
    double seconds = 8;
    pid_t pid = 0;
    bool self_test = false;
    const char* game_exe = nullptr;
    for (int i = 1; i < argc; ++i) {
        const std::string a = argv[i];
        if (a == "--role" && i + 1 < argc) role = argv[++i];
        else if (a == "--hz" && i + 1 < argc) hz = std::atof(argv[++i]);
        else if (a == "--seconds" && i + 1 < argc) seconds = std::atof(argv[++i]);
        else if (a == "--pid" && i + 1 < argc) pid = static_cast<pid_t>(std::atoi(argv[++i]));
        else if (a == "--self-test") self_test = true;
        else if (a == "--game-exe" && i + 1 < argc) game_exe = argv[++i];
    }
    if (role == "fake-sound") return fake_sound(hz);
    if (role == "fake-game") return fake_game(argv[0], hz);
    if (!self_test && pid == 0) {
        std::fprintf(stderr, "usage: %s --role probe (--self-test | --pid <game pid>) [--seconds N]\n", argv[0]);
        return 2;
    }
    return probe(argv[0], game_exe, pid, seconds, self_test);
}
