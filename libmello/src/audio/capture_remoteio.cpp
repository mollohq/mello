#include <TargetConditionals.h>
#if defined(__APPLE__) && TARGET_OS_IPHONE
#include "capture_remoteio.hpp"
#include "audio_session_ios.hpp"
#include "../util/log.hpp"
#include <cstdlib>

namespace mello::audio {

RemoteIOCapture::RemoteIOCapture() = default;

RemoteIOCapture::~RemoteIOCapture() {
    stop();
    if (audio_unit_) {
        AudioComponentInstanceDispose(audio_unit_);
        audio_unit_ = nullptr;
    }
    if (buffer_list_) {
        for (UInt32 i = 0; i < buffer_list_->mNumberBuffers; ++i) {
            free(buffer_list_->mBuffers[i].mData);
        }
        free(buffer_list_);
        buffer_list_ = nullptr;
    }
}

bool RemoteIOCapture::initialize(const char* /*device_id*/) {
    // iOS has no device selection; the route is owned by AVAudioSession. Activate
    // it before instantiating the unit so RemoteIO binds to the live route.
    if (!configure_voice_session()) {
        MELLO_LOG_ERROR("capture", "RemoteIO: audio session not active");
        return false;
    }

    AudioComponentDescription desc = {};
    desc.componentType = kAudioUnitType_Output;
    desc.componentSubType = kAudioUnitSubType_RemoteIO;
    desc.componentManufacturer = kAudioUnitManufacturer_Apple;

    AudioComponent component = AudioComponentFindNext(nullptr, &desc);
    if (!component) {
        MELLO_LOG_ERROR("capture", "RemoteIO: component not found");
        return false;
    }

    OSStatus status = AudioComponentInstanceNew(component, &audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "RemoteIO: AudioComponentInstanceNew failed: %d", (int)status);
        return false;
    }

    // Enable input on bus 1 (mic), disable output on bus 0 (this unit only captures).
    UInt32 enableIO = 1;
    status = AudioUnitSetProperty(audio_unit_,
        kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Input,
        1, &enableIO, sizeof(enableIO));
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "RemoteIO: enable input failed: %d", (int)status);
        return false;
    }
    UInt32 disableIO = 0;
    status = AudioUnitSetProperty(audio_unit_,
        kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Output,
        0, &disableIO, sizeof(disableIO));
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "RemoteIO: disable output failed: %d", (int)status);
        return false;
    }

    // Client format on the output scope of the input bus = what we receive.
    // 48 kHz mono int16 (our contract); RemoteIO resamples from the hardware rate.
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
        kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output,
        1, &format, sizeof(format));
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "RemoteIO: set format failed: %d", (int)status);
        return false;
    }

    sample_rate_ = 48000;
    channels_ = 1;

    UInt32 maxFrames = 0;
    UInt32 propSize = sizeof(maxFrames);
    AudioUnitGetProperty(audio_unit_,
        kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global,
        0, &maxFrames, &propSize);
    if (maxFrames == 0) maxFrames = 4096;

    buffer_list_ = (AudioBufferList*)calloc(1, sizeof(AudioBufferList));
    buffer_list_->mNumberBuffers = 1;
    buffer_list_->mBuffers[0].mNumberChannels = 1;
    buffer_list_->mBuffers[0].mDataByteSize = maxFrames * sizeof(int16_t);
    buffer_list_->mBuffers[0].mData = calloc(maxFrames, sizeof(int16_t));

    AURenderCallbackStruct cb = {};
    cb.inputProc = RemoteIOCapture::input_callback;
    cb.inputProcRefCon = this;
    status = AudioUnitSetProperty(audio_unit_,
        kAudioOutputUnitProperty_SetInputCallback, kAudioUnitScope_Global,
        0, &cb, sizeof(cb));
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "RemoteIO: set input callback failed: %d", (int)status);
        return false;
    }

    status = AudioUnitInitialize(audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "RemoteIO: AudioUnitInitialize failed: %d", (int)status);
        return false;
    }

    MELLO_LOG_INFO("capture", "RemoteIO: initialized (rate=%u ch=%u maxFrames=%u)",
                   sample_rate_, channels_, maxFrames);
    return true;
}

bool RemoteIOCapture::start(Callback callback) {
    if (running_) return false;
    callback_ = std::move(callback);
    running_ = true;

    OSStatus status = AudioOutputUnitStart(audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "RemoteIO: start failed: %d", (int)status);
        running_ = false;
        return false;
    }
    // Resume capture after an interruption ends (the OS stops the unit).
    register_audio_restart(this, [this] {
        if (running_) AudioOutputUnitStart(audio_unit_);
    });
    MELLO_LOG_INFO("capture", "RemoteIO: capture started");
    return true;
}

void RemoteIOCapture::stop() {
    if (!running_) return;
    running_ = false;
    unregister_audio_restart(this);
    if (audio_unit_) {
        AudioOutputUnitStop(audio_unit_);
    }
    MELLO_LOG_INFO("capture", "RemoteIO: capture stopped");
}

OSStatus RemoteIOCapture::input_callback(
    void* inRefCon,
    AudioUnitRenderActionFlags* ioActionFlags,
    const AudioTimeStamp* inTimeStamp,
    UInt32 inBusNumber,
    UInt32 inNumberFrames,
    AudioBufferList* /* ioData */)
{
    auto* self = static_cast<RemoteIOCapture*>(inRefCon);
    if (!self->running_ || !self->callback_) return noErr;

    self->buffer_list_->mBuffers[0].mDataByteSize = inNumberFrames * sizeof(int16_t);

    OSStatus status = AudioUnitRender(self->audio_unit_,
        ioActionFlags, inTimeStamp, inBusNumber, inNumberFrames, self->buffer_list_);
    if (status != noErr) return status;

    const int16_t* samples = static_cast<const int16_t*>(self->buffer_list_->mBuffers[0].mData);
    self->callback_(samples, static_cast<size_t>(inNumberFrames));
    return noErr;
}

} // namespace mello::audio
#endif
