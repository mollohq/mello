#ifdef __APPLE__
#include "playback_coreaudio.hpp"
#include "../util/log.hpp"
#include <cstring>
#include <vector>
#include <cmath>

namespace mello::audio {

CoreAudioPlayback::CoreAudioPlayback() = default;

CoreAudioPlayback::~CoreAudioPlayback() {
    stop();
    if (audio_unit_) {
        AudioComponentInstanceDispose(audio_unit_);
        audio_unit_ = nullptr;
    }
}

bool CoreAudioPlayback::initialize(const char* device_id) {
    MELLO_LOG_INFO("playback", "CoreAudio: initializing (device=%s)", device_id ? device_id : "default");

    // Find the default output Audio Unit (AUHAL)
    AudioComponentDescription desc = {};
    desc.componentType = kAudioUnitType_Output;
    desc.componentSubType = kAudioUnitSubType_HALOutput;
    desc.componentManufacturer = kAudioUnitManufacturer_Apple;

    AudioComponent component = AudioComponentFindNext(nullptr, &desc);
    if (!component) {
        MELLO_LOG_ERROR("playback", "CoreAudio: AUHAL component not found");
        return false;
    }

    OSStatus status = AudioComponentInstanceNew(component, &audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "CoreAudio: AudioComponentInstanceNew failed: %d", (int)status);
        return false;
    }

    // Set the output device
    if (device_id && device_id[0] != '\0') {
        device_id_ = static_cast<AudioDeviceID>(std::stoul(device_id));
    } else {
        AudioObjectPropertyAddress prop = {
            kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain
        };
        UInt32 size = sizeof(device_id_);
        status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &prop, 0, nullptr, &size, &device_id_);
        if (status != noErr || device_id_ == kAudioObjectUnknown) {
            MELLO_LOG_ERROR("playback", "CoreAudio: no default output device");
            return false;
        }
    }

    status = AudioUnitSetProperty(audio_unit_,
        kAudioOutputUnitProperty_CurrentDevice,
        kAudioUnitScope_Global,
        0,
        &device_id_, sizeof(device_id_));
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "CoreAudio: set device failed: %d", (int)status);
        return false;
    }

    // Query the device's native stream format to find out channel count
    AudioStreamBasicDescription deviceFormat = {};
    UInt32 fmtSize = sizeof(deviceFormat);
    status = AudioUnitGetProperty(audio_unit_,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Output, // physical output
        0,
        &deviceFormat, &fmtSize);
    if (status == noErr) {
        device_channels_ = deviceFormat.mChannelsPerFrame;
        if (device_channels_ == 0) device_channels_ = 2;
    }

    // Set our input format on bus 0: 48kHz mono 16-bit integer PCM
    // CoreAudio will convert to the device format for us
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
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Input, // input to output bus = what we provide
        0, // output bus
        &format, sizeof(format));
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "CoreAudio: set format failed: %d", (int)status);
        return false;
    }

    AudioStreamBasicDescription actual = {};
    UInt32 actual_size = sizeof(actual);
    status = AudioUnitGetProperty(audio_unit_,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Input,
        0,
        &actual, &actual_size);
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "CoreAudio: get actual format failed: %d", (int)status);
        return false;
    }
    if (std::fabs(actual.mSampleRate - 48000.0) > 1.0 ||
        actual.mChannelsPerFrame != 1 ||
        actual.mBitsPerChannel != 16) {
        MELLO_LOG_ERROR(
            "playback",
            "CoreAudio: format contract mismatch actual(rate=%.1f ch=%u bits=%u)",
            actual.mSampleRate,
            (unsigned)actual.mChannelsPerFrame,
            (unsigned)actual.mBitsPerChannel);
        return false;
    }

    sample_rate_ = 48000;

    // Set the render callback
    AURenderCallbackStruct callbackStruct = {};
    callbackStruct.inputProc = CoreAudioPlayback::render_callback;
    callbackStruct.inputProcRefCon = this;

    status = AudioUnitSetProperty(audio_unit_,
        kAudioUnitProperty_SetRenderCallback,
        kAudioUnitScope_Input,
        0, // output bus
        &callbackStruct, sizeof(callbackStruct));
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "CoreAudio: set render callback failed: %d", (int)status);
        return false;
    }

    status = AudioUnitInitialize(audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "CoreAudio: AudioUnitInitialize failed: %d", (int)status);
        return false;
    }

    MELLO_LOG_INFO("playback", "CoreAudio: initialized (rate=%u device_ch=%u device=%u)",
                   sample_rate_, device_channels_, (unsigned)device_id_);
    return true;
}

bool CoreAudioPlayback::start() {
    if (running_) return false;
    running_ = true;

    OSStatus status = AudioOutputUnitStart(audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("playback", "CoreAudio: start failed: %d", (int)status);
        running_ = false;
        return false;
    }

    MELLO_LOG_INFO("playback", "CoreAudio: playback started");
    return true;
}

void CoreAudioPlayback::stop() {
    if (!running_) return;
    running_ = false;
    if (audio_unit_) {
        AudioOutputUnitStop(audio_unit_);
    }
    MELLO_LOG_INFO("playback", "CoreAudio: playback stopped");
}

size_t CoreAudioPlayback::feed(const int16_t* samples, size_t count) {
    return ring_.write(samples, count);
}

OSStatus CoreAudioPlayback::render_callback(
    void* inRefCon,
    AudioUnitRenderActionFlags* ioActionFlags,
    const AudioTimeStamp* /* inTimeStamp */,
    UInt32 /* inBusNumber */,
    UInt32 inNumberFrames,
    AudioBufferList* ioData)
{
    auto* self = static_cast<CoreAudioPlayback*>(inRefCon);
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
