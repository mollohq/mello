#include "audio_session_ios.hpp"
#include "../util/log.hpp"
#import <AVFoundation/AVFoundation.h>

namespace mello::audio {

bool configure_voice_session() {
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

} // namespace mello::audio
