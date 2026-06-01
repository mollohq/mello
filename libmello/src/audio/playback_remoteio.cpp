#include <TargetConditionals.h>
#if defined(__APPLE__) && TARGET_OS_IPHONE
#include "playback_remoteio.hpp"
#include "audio_session_ios.hpp"
#include "../util/log.hpp"
#include <cstring>

namespace mello::audio {

RemoteIOPlayback::RemoteIOPlayback() = default;

RemoteIOPlayback::~RemoteIOPlayback() {
    stop();
    if (audio_unit_) {
        AudioComponentInstanceDispose(audio_unit_);
        audio_unit_ = nullptr;
    }
}

bool RemoteIOPlayback::initialize(const char* /*device_id*/) {
    if (!configure_voice_session()) {
        MELLO_LOG_ERROR("playback", "RemoteIO: audio session not active");
        return false;
    }

    AudioComponentDescription desc = {};
    desc.componentType = kAudioUnitType_Output;
    desc.componentSubType = kAudioUnitSubType_RemoteIO;
    desc.componentManufacturer = kAudioUnitManufacturer_Apple;

    AudioComponent component = AudioComponentFindNext(nullptr, &desc);
    if (!component) {
        MELLO_LOG_ERROR("playback", "RemoteIO: component not found");
        return false;
    }

    OSStatus status = AudioComponentInstanceNew(component, &audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "RemoteIO: AudioComponentInstanceNew failed: %d", (int)status);
        return false;
    }

    // Client format on the input scope of the output bus = what we provide.
    // 48 kHz mono int16; RemoteIO converts to the hardware format on the way out.
    AudioStreamBasicDescription format = {};
    format.mSampleRate = 48000.0;
    format.mFormatID = kAudioFormatLinearPCM;
    format.mFormatFlags = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked;
    format.mBitsPerChannel = 16;
    format.mChannelsPerFrame = 1;
    format.mFramesPerPacket = 1;
    format.mBytesPerFrame = format.mChannelsPerFrame * (format.mBitsPerChannel / 8);
    format.mBytesPerPacket = format.mBytesPerFrame * format.mFramesPerPacket;

    status = AudioUnitSetProperty(audio_unit_,
        kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input,
        0, &format, sizeof(format));
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "RemoteIO: set format failed: %d", (int)status);
        return false;
    }

    sample_rate_ = 48000;

    AURenderCallbackStruct cb = {};
    cb.inputProc = RemoteIOPlayback::render_callback;
    cb.inputProcRefCon = this;
    status = AudioUnitSetProperty(audio_unit_,
        kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input,
        0, &cb, sizeof(cb));
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "RemoteIO: set render callback failed: %d", (int)status);
        return false;
    }

    status = AudioUnitInitialize(audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "RemoteIO: AudioUnitInitialize failed: %d", (int)status);
        return false;
    }

    MELLO_LOG_INFO("playback", "RemoteIO: initialized (rate=%u)", sample_rate_);
    return true;
}

bool RemoteIOPlayback::start() {
    if (running_) return false;
    running_ = true;

    OSStatus status = AudioOutputUnitStart(audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "RemoteIO: start failed: %d", (int)status);
        running_ = false;
        return false;
    }
    // Resume playback after an interruption ends (the OS stops the unit).
    register_audio_restart(this, [this] {
        if (running_) AudioOutputUnitStart(audio_unit_);
    });
    MELLO_LOG_INFO("playback", "RemoteIO: playback started");
    return true;
}

void RemoteIOPlayback::stop() {
    if (!running_) return;
    running_ = false;
    unregister_audio_restart(this);
    if (audio_unit_) {
        AudioOutputUnitStop(audio_unit_);
    }
    MELLO_LOG_INFO("playback", "RemoteIO: playback stopped");
}

size_t RemoteIOPlayback::feed(const int16_t* samples, size_t count) {
    return ring_.write(samples, count);
}

OSStatus RemoteIOPlayback::render_callback(
    void* inRefCon,
    AudioUnitRenderActionFlags* /* ioActionFlags */,
    const AudioTimeStamp* /* inTimeStamp */,
    UInt32 /* inBusNumber */,
    UInt32 inNumberFrames,
    AudioBufferList* ioData)
{
    auto* self = static_cast<RemoteIOPlayback*>(inRefCon);
    int16_t* out = static_cast<int16_t*>(ioData->mBuffers[0].mData);

    size_t got = 0;
    if (self->render_source_) {
        got = self->render_source_(out, inNumberFrames);
    } else {
        got = self->ring_.read(out, inNumberFrames);
    }

    if (got < inNumberFrames) {
        std::memset(out + got, 0, (inNumberFrames - got) * sizeof(int16_t));
    }
    return noErr;
}

} // namespace mello::audio
#endif
