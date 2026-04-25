#ifdef __APPLE__
#include "capture_coreaudio.hpp"
#include "../util/log.hpp"
#include <cstring>
#include <cmath>

namespace mello::audio {

CoreAudioCapture::CoreAudioCapture() = default;

CoreAudioCapture::~CoreAudioCapture() {
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

bool CoreAudioCapture::initialize(const char* device_id) {
    MELLO_LOG_INFO("capture", "CoreAudio: initializing (device=%s)", device_id ? device_id : "default");

    // Find the AUHAL component
    AudioComponentDescription desc = {};
    desc.componentType = kAudioUnitType_Output;
    desc.componentSubType = kAudioUnitSubType_HALOutput;
    desc.componentManufacturer = kAudioUnitManufacturer_Apple;

    AudioComponent component = AudioComponentFindNext(nullptr, &desc);
    if (!component) {
        MELLO_LOG_ERROR("capture", "CoreAudio: AUHAL component not found");
        return false;
    }

    OSStatus status = AudioComponentInstanceNew(component, &audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "CoreAudio: AudioComponentInstanceNew failed: %d", (int)status);
        return false;
    }

    // Enable input on bus 1 (input bus)
    UInt32 enableIO = 1;
    status = AudioUnitSetProperty(audio_unit_,
        kAudioOutputUnitProperty_EnableIO,
        kAudioUnitScope_Input,
        1, // input bus
        &enableIO, sizeof(enableIO));
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "CoreAudio: enable input failed: %d", (int)status);
        return false;
    }

    // Disable output on bus 0 (we only capture, don't play)
    UInt32 disableIO = 0;
    status = AudioUnitSetProperty(audio_unit_,
        kAudioOutputUnitProperty_EnableIO,
        kAudioUnitScope_Output,
        0, // output bus
        &disableIO, sizeof(disableIO));
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "CoreAudio: disable output failed: %d", (int)status);
        return false;
    }

    // Set the capture device
    if (device_id && device_id[0] != '\0') {
        try {
            device_id_ = static_cast<AudioDeviceID>(std::stoul(device_id));
        } catch (...) {
            MELLO_LOG_ERROR("capture", "CoreAudio: invalid device id '%s'", device_id);
            return false;
        }
    } else {
        // Get default input device
        AudioObjectPropertyAddress prop = {
            kAudioHardwarePropertyDefaultInputDevice,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain
        };
        UInt32 size = sizeof(device_id_);
        status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &prop, 0, nullptr, &size, &device_id_);
        if (status != noErr || device_id_ == kAudioObjectUnknown) {
            MELLO_LOG_ERROR("capture", "CoreAudio: no default input device");
            return false;
        }
    }

    status = AudioUnitSetProperty(audio_unit_,
        kAudioOutputUnitProperty_CurrentDevice,
        kAudioUnitScope_Global,
        0,
        &device_id_, sizeof(device_id_));
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "CoreAudio: set device failed: %d", (int)status);
        return false;
    }

    // Set desired format: 48kHz mono 16-bit integer PCM
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
        kAudioUnitScope_Output, // output of input bus = what we receive
        1, // input bus
        &format, sizeof(format));
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "CoreAudio: set format failed: %d", (int)status);
        return false;
    }

    AudioStreamBasicDescription actual = {};
    UInt32 actual_size = sizeof(actual);
    status = AudioUnitGetProperty(audio_unit_,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Output,
        1,
        &actual, &actual_size);
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "CoreAudio: get actual format failed: %d", (int)status);
        return false;
    }
    if (std::fabs(actual.mSampleRate - 48000.0) > 1.0 ||
        actual.mChannelsPerFrame != 1 ||
        actual.mBitsPerChannel != 16) {
        MELLO_LOG_ERROR(
            "capture",
            "CoreAudio: format contract mismatch actual(rate=%.1f ch=%u bits=%u)",
            actual.mSampleRate,
            (unsigned)actual.mChannelsPerFrame,
            (unsigned)actual.mBitsPerChannel);
        return false;
    }

    sample_rate_ = 48000;
    channels_ = 1;

    // Get the max frames the AU will deliver per callback
    UInt32 maxFrames = 0;
    UInt32 propSize = sizeof(maxFrames);
    AudioUnitGetProperty(audio_unit_,
        kAudioUnitProperty_MaximumFramesPerSlice,
        kAudioUnitScope_Global,
        0, &maxFrames, &propSize);
    if (maxFrames == 0) maxFrames = 4096;

    // Allocate buffer list for the render call
    buffer_list_ = (AudioBufferList*)calloc(1, sizeof(AudioBufferList));
    buffer_list_->mNumberBuffers = 1;
    buffer_list_->mBuffers[0].mNumberChannels = 1;
    buffer_list_->mBuffers[0].mDataByteSize = maxFrames * sizeof(int16_t);
    buffer_list_->mBuffers[0].mData = calloc(maxFrames, sizeof(int16_t));

    render_buf_.resize(maxFrames);

    // Set input callback
    AURenderCallbackStruct callbackStruct = {};
    callbackStruct.inputProc = CoreAudioCapture::input_callback;
    callbackStruct.inputProcRefCon = this;

    status = AudioUnitSetProperty(audio_unit_,
        kAudioOutputUnitProperty_SetInputCallback,
        kAudioUnitScope_Global,
        0,
        &callbackStruct, sizeof(callbackStruct));
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "CoreAudio: set input callback failed: %d", (int)status);
        return false;
    }

    status = AudioUnitInitialize(audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "CoreAudio: AudioUnitInitialize failed: %d", (int)status);
        return false;
    }

    MELLO_LOG_INFO("capture", "CoreAudio: initialized (rate=%u ch=%u maxFrames=%u device=%u)",
                   sample_rate_, channels_, maxFrames, (unsigned)device_id_);
    return true;
}

bool CoreAudioCapture::start(Callback callback) {
    if (running_) return false;
    callback_ = std::move(callback);
    running_ = true;

    OSStatus status = AudioOutputUnitStart(audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("capture", "CoreAudio: start failed: %d", (int)status);
        running_ = false;
        return false;
    }

    MELLO_LOG_INFO("capture", "CoreAudio: capture started");
    return true;
}

void CoreAudioCapture::stop() {
    if (!running_) return;
    running_ = false;
    if (audio_unit_) {
        AudioOutputUnitStop(audio_unit_);
    }
    MELLO_LOG_INFO("capture", "CoreAudio: capture stopped");
}

OSStatus CoreAudioCapture::input_callback(
    void* inRefCon,
    AudioUnitRenderActionFlags* ioActionFlags,
    const AudioTimeStamp* inTimeStamp,
    UInt32 inBusNumber,
    UInt32 inNumberFrames,
    AudioBufferList* /* ioData */)
{
    auto* self = static_cast<CoreAudioCapture*>(inRefCon);
    if (!self->running_ || !self->callback_) return noErr;

    // Reset buffer for this render call
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
