#include "audio_session_ios.hpp"
#include "../util/log.hpp"
#import <AVFoundation/AVFoundation.h>
#include <algorithm>
#include <mutex>
#include <utility>
#include <vector>

namespace mello::audio {

namespace {

std::mutex g_restart_mutex;
std::vector<std::pair<void*, std::function<void()>>> g_restart_hooks;

void restart_all_units() {
    std::vector<std::function<void()>> hooks;
    {
        std::lock_guard<std::mutex> lock(g_restart_mutex);
        for (auto& [token, fn] : g_restart_hooks) hooks.push_back(fn);
    }
    for (auto& fn : hooks) {
        if (fn) fn();
    }
}

// Observe AVAudioSession interruptions once. On interruption end (with the
// system's resume hint) we reactivate the session and restart the IO units —
// without this, voice stays dead after an incoming phone call / Siri.
void install_session_observers() {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        [[NSNotificationCenter defaultCenter]
            addObserverForName:AVAudioSessionInterruptionNotification
                        object:nil
                         queue:nil
                    usingBlock:^(NSNotification* note) {
            NSInteger type = [note.userInfo[AVAudioSessionInterruptionTypeKey] integerValue];
            if (type == AVAudioSessionInterruptionTypeBegan) {
                MELLO_LOG_INFO("session", "iOS: audio interrupted");
                return;
            }
            NSInteger opts = [note.userInfo[AVAudioSessionInterruptionOptionKey] integerValue];
            bool resume = (opts & AVAudioSessionInterruptionOptionShouldResume) != 0;
            MELLO_LOG_INFO("session", "iOS: interruption ended (resume=%d)", (int)resume);
            if (resume && configure_voice_session()) {
                restart_all_units();
            }
        }];
    });
}

} // namespace

bool configure_voice_session() {
    install_session_observers();

    AVAudioSession* session = [AVAudioSession sharedInstance];
    NSError* err = nil;

    // .voiceChat tunes the route for two-way VoIP (echo-friendly, earpiece/speaker
    // routing). We run our own AEC3 in the pipeline, so plain RemoteIO + this
    // category is enough; .defaultToSpeaker keeps loopback audible without a headset.
    [session setCategory:AVAudioSessionCategoryPlayAndRecord
                    mode:AVAudioSessionModeVoiceChat
                 options:AVAudioSessionCategoryOptionAllowBluetooth |
                         AVAudioSessionCategoryOptionDefaultToSpeaker
                   error:&err];
    if (err) {
        MELLO_LOG_ERROR("session", "iOS: setCategory failed: %s",
                        err.localizedDescription.UTF8String);
        return false;
    }

    // Request 48 kHz to match our contract; the OS may pick a nearby rate, in which
    // case RemoteIO resamples to our client format. Best-effort (ignore failure).
    [session setPreferredSampleRate:48000.0 error:nil];

    [session setActive:YES error:&err];
    if (err) {
        MELLO_LOG_ERROR("session", "iOS: setActive failed: %s",
                        err.localizedDescription.UTF8String);
        return false;
    }

    MELLO_LOG_INFO("session", "iOS: audio session active (rate=%.0f)",
                   session.sampleRate);
    return true;
}

void deactivate_voice_session() {
    NSError* err = nil;
    [[AVAudioSession sharedInstance]
        setActive:NO
        withOptions:AVAudioSessionSetActiveOptionNotifyOthersOnDeactivation
        error:&err];
    if (err) {
        MELLO_LOG_WARN("session", "iOS: setActive:NO failed: %s",
                       err.localizedDescription.UTF8String);
    }
}

void register_audio_restart(void* token, std::function<void()> restart) {
    std::lock_guard<std::mutex> lock(g_restart_mutex);
    g_restart_hooks.emplace_back(token, std::move(restart));
}

void unregister_audio_restart(void* token) {
    std::lock_guard<std::mutex> lock(g_restart_mutex);
    g_restart_hooks.erase(
        std::remove_if(g_restart_hooks.begin(), g_restart_hooks.end(),
                       [token](const auto& p) { return p.first == token; }),
        g_restart_hooks.end());
}

} // namespace mello::audio
