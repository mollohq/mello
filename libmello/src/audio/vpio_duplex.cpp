#ifdef __APPLE__
#include "vpio_duplex.hpp"
#include "coreaudio_unit_lock.hpp"
#include "../util/log.hpp"
#include <cmath>
#include <cstring>

namespace mello::audio {

namespace {

// Empty/null id follows the system default device for the given selector.
bool resolve_device_id(const char* device_id,
                       AudioObjectPropertySelector default_selector,
                       AudioDeviceID& out) {
    if (device_id && device_id[0] != '\0') {
        try {
            out = static_cast<AudioDeviceID>(std::stoul(device_id));
        } catch (...) {
            MELLO_LOG_ERROR("vpio", "invalid device id '%s'", device_id);
            return false;
        }
        return true;
    }
    AudioObjectPropertyAddress prop = {default_selector,
                                       kAudioObjectPropertyScopeGlobal,
                                       kAudioObjectPropertyElementMain};
    UInt32 size = sizeof(out);
    OSStatus status = AudioObjectGetPropertyData(
        kAudioObjectSystemObject, &prop, 0, nullptr, &size, &out);
    if (status != noErr || out == kAudioObjectUnknown) {
        MELLO_LOG_ERROR("vpio", "no default device (selector %u)", (unsigned)default_selector);
        return false;
    }
    return true;
}

void set_format_48k_mono_int16(AudioStreamBasicDescription& format) {
    format.mSampleRate = 48000.0;
    format.mFormatID = kAudioFormatLinearPCM;
    format.mFormatFlags = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked;
    format.mBitsPerChannel = 16;
    format.mChannelsPerFrame = 1;
    format.mFramesPerPacket = 1;
    format.mBytesPerFrame = 2;
    format.mBytesPerPacket = 2;
}

}  // namespace

VpioUnit::~VpioUnit() {
    shutdown();
}

bool VpioUnit::initialize(const char* capture_device_id, const char* playback_device_id) {
    // Unit setup is serialized process-wide (see coreaudio_unit_lock.hpp).
    std::lock_guard<std::mutex> lock(coreaudio_unit_mutex());
    if (audio_unit_) return true;
    MELLO_LOG_INFO("vpio", "initializing (capture=%s playback=%s)",
                   capture_device_id && capture_device_id[0] ? capture_device_id : "default",
                   playback_device_id && playback_device_id[0] ? playback_device_id : "default");

    if (!resolve_device_id(capture_device_id,
                           kAudioHardwarePropertyDefaultInputDevice, input_device_)) {
        return false;
    }
    if (!resolve_device_id(playback_device_id,
                           kAudioHardwarePropertyDefaultOutputDevice, output_device_)) {
        return false;
    }

    AudioComponentDescription desc = {};
    desc.componentType = kAudioUnitType_Output;
    desc.componentSubType = kAudioUnitSubType_VoiceProcessingIO;
    desc.componentManufacturer = kAudioUnitManufacturer_Apple;

    AudioComponent component = AudioComponentFindNext(nullptr, &desc);
    if (!component) {
        MELLO_LOG_ERROR("vpio", "VoiceProcessingIO component not found");
        return false;
    }

    OSStatus status = AudioComponentInstanceNew(component, &audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "AudioComponentInstanceNew failed: %d", (int)status);
        audio_unit_ = nullptr;
        return false;
    }

    UInt32 enableIO = 1;
    status = AudioUnitSetProperty(audio_unit_, kAudioOutputUnitProperty_EnableIO,
                                  kAudioUnitScope_Input, 1, &enableIO, sizeof(enableIO));
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "enable input failed: %d", (int)status);
        shutdown_locked();
        return false;
    }
    status = AudioUnitSetProperty(audio_unit_, kAudioOutputUnitProperty_EnableIO,
                                  kAudioUnitScope_Output, 0, &enableIO, sizeof(enableIO));
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "enable output failed: %d", (int)status);
        shutdown_locked();
        return false;
    }

    status = AudioUnitSetProperty(audio_unit_, kAudioOutputUnitProperty_CurrentDevice,
                                  kAudioUnitScope_Global, 0,
                                  &output_device_, sizeof(output_device_));
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "set output device failed: %d", (int)status);
        shutdown_locked();
        return false;
    }
    status = AudioUnitSetProperty(audio_unit_, kAudioOutputUnitProperty_CurrentDevice,
                                  kAudioUnitScope_Input, 1,
                                  &input_device_, sizeof(input_device_));
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "set input device failed: %d", (int)status);
        shutdown_locked();
        return false;
    }

    AudioStreamBasicDescription format = {};
    set_format_48k_mono_int16(format);
    status = AudioUnitSetProperty(audio_unit_, kAudioUnitProperty_StreamFormat,
                                  kAudioUnitScope_Output, 1, &format, sizeof(format));
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "set input format failed: %d", (int)status);
        shutdown_locked();
        return false;
    }
    status = AudioUnitSetProperty(audio_unit_, kAudioUnitProperty_StreamFormat,
                                  kAudioUnitScope_Input, 0, &format, sizeof(format));
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "set output format failed: %d", (int)status);
        shutdown_locked();
        return false;
    }

    // Contract validation: the pipeline DSP runs 48k mono int16 throughout.
    AudioStreamBasicDescription actual = {};
    UInt32 actual_size = sizeof(actual);
    status = AudioUnitGetProperty(audio_unit_, kAudioUnitProperty_StreamFormat,
                                  kAudioUnitScope_Output, 1, &actual, &actual_size);
    if (status != noErr || std::fabs(actual.mSampleRate - 48000.0) > 1.0 ||
        actual.mChannelsPerFrame != 1 || actual.mBitsPerChannel != 16) {
        MELLO_LOG_ERROR("vpio", "input format contract mismatch (rate=%.1f ch=%u bits=%u)",
                        actual.mSampleRate, (unsigned)actual.mChannelsPerFrame,
                        (unsigned)actual.mBitsPerChannel);
        shutdown_locked();
        return false;
    }
    actual_size = sizeof(actual);
    status = AudioUnitGetProperty(audio_unit_, kAudioUnitProperty_StreamFormat,
                                  kAudioUnitScope_Input, 0, &actual, &actual_size);
    if (status != noErr || std::fabs(actual.mSampleRate - 48000.0) > 1.0 ||
        actual.mChannelsPerFrame != 1 || actual.mBitsPerChannel != 16) {
        MELLO_LOG_ERROR("vpio", "output format contract mismatch (rate=%.1f ch=%u bits=%u)",
                        actual.mSampleRate, (unsigned)actual.mChannelsPerFrame,
                        (unsigned)actual.mBitsPerChannel);
        shutdown_locked();
        return false;
    }
    sample_rate_ = 48000;

    UInt32 maxFrames = 0;
    UInt32 propSize = sizeof(maxFrames);
    AudioUnitGetProperty(audio_unit_, kAudioUnitProperty_MaximumFramesPerSlice,
                         kAudioUnitScope_Global, 0, &maxFrames, &propSize);
    if (maxFrames == 0) maxFrames = 4096;

    capture_buffer_list_ =
        static_cast<AudioBufferList*>(calloc(1, sizeof(AudioBufferList)));
    capture_buffer_list_->mNumberBuffers = 1;
    capture_buffer_list_->mBuffers[0].mNumberChannels = 1;
    capture_buffer_list_->mBuffers[0].mDataByteSize = maxFrames * sizeof(int16_t);
    capture_buffer_list_->mBuffers[0].mData = calloc(maxFrames, sizeof(int16_t));

    AURenderCallbackStruct inputCb = {};
    inputCb.inputProc = VpioUnit::input_callback;
    inputCb.inputProcRefCon = this;
    status = AudioUnitSetProperty(audio_unit_, kAudioOutputUnitProperty_SetInputCallback,
                                  kAudioUnitScope_Global, 0, &inputCb, sizeof(inputCb));
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "set input callback failed: %d", (int)status);
        shutdown_locked();
        return false;
    }

    AURenderCallbackStruct renderCb = {};
    renderCb.inputProc = VpioUnit::render_callback;
    renderCb.inputProcRefCon = this;
    status = AudioUnitSetProperty(audio_unit_, kAudioUnitProperty_SetRenderCallback,
                                  kAudioUnitScope_Input, 0, &renderCb, sizeof(renderCb));
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "set render callback failed: %d", (int)status);
        shutdown_locked();
        return false;
    }

    status = AudioUnitInitialize(audio_unit_);
    if (status != noErr) {
        MELLO_LOG_ERROR("vpio", "AudioUnitInitialize failed: %d", (int)status);
        shutdown_locked();
        return false;
    }

    cached_input_latency_ms_ =
        query_latency_ms(kAudioObjectPropertyScopeInput, input_device_);
    cached_output_latency_ms_ =
        query_latency_ms(kAudioObjectPropertyScopeOutput, output_device_);

    MELLO_LOG_INFO("vpio", "initialized (in=%u out=%u maxFrames=%u in_lat=%dms out_lat=%dms)",
                   (unsigned)input_device_, (unsigned)output_device_, maxFrames,
                   cached_input_latency_ms_, cached_output_latency_ms_);
    return true;
}

void VpioUnit::shutdown() {
    // Idempotent: safe from the destructor, init-failure paths, and teardown.
    // Init-failure paths run under coreaudio_unit_mutex(); take it here only
    // when not already held — split into shutdown_locked.
    std::lock_guard<std::mutex> lock(coreaudio_unit_mutex());
    shutdown_locked();
}

void VpioUnit::shutdown_locked() {
    if (audio_unit_) {
        AudioOutputUnitStop(audio_unit_);
        AudioComponentInstanceDispose(audio_unit_);
        audio_unit_ = nullptr;
    }
    if (capture_buffer_list_) {
        free(capture_buffer_list_->mBuffers[0].mData);
        free(capture_buffer_list_);
        capture_buffer_list_ = nullptr;
    }
    std::lock_guard<std::mutex> ref_lock(mutex_);
    input_users_ = 0;
    output_users_ = 0;
    capturing_.store(false, std::memory_order_relaxed);
    MELLO_LOG_INFO("vpio", "shut down");
}

int VpioUnit::query_latency_ms(AudioObjectPropertyScope scope, AudioDeviceID device) const {
    double total_ms = 0.0;

    Float64 unit_latency_sec = 0.0;
    UInt32 size = sizeof(unit_latency_sec);
    OSStatus s = AudioUnitGetProperty(audio_unit_, kAudioUnitProperty_Latency,
                                      kAudioUnitScope_Global, 0,
                                      &unit_latency_sec, &size);
    if (s == noErr && unit_latency_sec > 0 && unit_latency_sec < 2.0) {
        total_ms += unit_latency_sec * 1000.0;
    }

    if (device != kAudioObjectUnknown) {
        UInt32 safety_frames = 0;
        size = sizeof(safety_frames);
        AudioObjectPropertyAddress safety_addr = {kAudioDevicePropertySafetyOffset, scope,
                                                  kAudioObjectPropertyElementMain};
        if (AudioObjectHasProperty(device, &safety_addr) &&
            AudioObjectGetPropertyData(device, &safety_addr, 0, nullptr,
                                       &size, &safety_frames) == noErr) {
            total_ms += static_cast<double>(safety_frames) * 1000.0 / 48000.0;
        }
        UInt32 buffer_frames = 0;
        size = sizeof(buffer_frames);
        AudioObjectPropertyAddress buf_addr = {kAudioDevicePropertyBufferFrameSize, scope,
                                               kAudioObjectPropertyElementMain};
        if (AudioObjectHasProperty(device, &buf_addr) &&
            AudioObjectGetPropertyData(device, &buf_addr, 0, nullptr,
                                       &size, &buffer_frames) == noErr &&
            buffer_frames > 0 && buffer_frames <= 8192) {
            total_ms += static_cast<double>(buffer_frames) * 1000.0 / 48000.0;
        }
    }

    if (total_ms < 0) total_ms = 0;
    if (total_ms > 500) total_ms = 500;
    return static_cast<int>(total_ms + 0.5);
}

bool VpioUnit::start_capture(AudioCapture::Callback callback) {
    if (!audio_unit_) return false;
    std::lock_guard<std::mutex> lock(mutex_);
    if (capturing_.load(std::memory_order_relaxed)) return false;
    capture_callback_ = std::move(callback);
    capturing_.store(true, std::memory_order_relaxed);
    // 0 -> 1 transition starts the unit; ref_unit_locked is gone —
    // increment-then-check would never observe the transition.
    const bool should_start = (input_users_ + output_users_) == 0;
    input_users_++;
    if (should_start) {
        OSStatus status = AudioOutputUnitStart(audio_unit_);
        if (status != noErr) {
            MELLO_LOG_ERROR("vpio", "start failed: %d", (int)status);
        } else {
            MELLO_LOG_INFO("vpio", "unit started");
        }
    }
    MELLO_LOG_INFO("vpio", "capture started");
    return true;
}

void VpioUnit::stop_capture() {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!capturing_.load(std::memory_order_relaxed)) return;
    capturing_.store(false, std::memory_order_relaxed);
    capture_callback_ = nullptr;
    if (input_users_ > 0) input_users_--;
    if (input_users_ + output_users_ == 0 && audio_unit_) {
        AudioOutputUnitStop(audio_unit_);
        MELLO_LOG_INFO("vpio", "unit stopped (no users)");
    }
    MELLO_LOG_INFO("vpio", "capture stopped");
}

void VpioUnit::set_render_source(RenderSourceFn fn) {
    render_source_ = std::move(fn);
}

bool VpioUnit::start_playback() {
    if (!audio_unit_) return false;
    std::lock_guard<std::mutex> lock(mutex_);
    const bool should_start = (input_users_ + output_users_) == 0;
    output_users_++;
    if (should_start) {
        OSStatus status = AudioOutputUnitStart(audio_unit_);
        if (status != noErr) {
            MELLO_LOG_ERROR("vpio", "start failed: %d", (int)status);
        } else {
            MELLO_LOG_INFO("vpio", "unit started");
        }
    }
    MELLO_LOG_INFO("vpio", "playback started");
    return true;
}

void VpioUnit::stop_playback() {
    std::lock_guard<std::mutex> lock(mutex_);
    if (output_users_ > 0) output_users_--;
    if (input_users_ + output_users_ == 0 && audio_unit_) {
        AudioOutputUnitStop(audio_unit_);
        MELLO_LOG_INFO("vpio", "unit stopped (no users)");
    }
    MELLO_LOG_INFO("vpio", "playback stopped");
}

size_t VpioUnit::feed(const int16_t* samples, size_t count) {
    return ring_.write(samples, count);
}

OSStatus VpioUnit::input_callback(void* inRefCon,
                                 AudioUnitRenderActionFlags* ioActionFlags,
                                 const AudioTimeStamp* inTimeStamp,
                                 UInt32 inBusNumber,
                                 UInt32 inNumberFrames,
                                 AudioBufferList* /* ioData */) {
    auto* self = static_cast<VpioUnit*>(inRefCon);
    if (!self->capturing_.load(std::memory_order_relaxed)) return noErr;
    AudioCapture::Callback cb;
    {
        // Copy under lock; invoke without it (no locks on realtime paths).
        std::lock_guard<std::mutex> lock(self->mutex_);
        if (!self->capturing_.load(std::memory_order_relaxed)) return noErr;
        cb = self->capture_callback_;
    }
    if (!cb || !self->capture_buffer_list_) return noErr;

    self->capture_buffer_list_->mBuffers[0].mDataByteSize =
        inNumberFrames * sizeof(int16_t);
    OSStatus status = AudioUnitRender(self->audio_unit_, ioActionFlags, inTimeStamp,
                                      inBusNumber, inNumberFrames,
                                      self->capture_buffer_list_);
    if (status != noErr) return status;

    const int16_t* samples =
        static_cast<const int16_t*>(self->capture_buffer_list_->mBuffers[0].mData);
    cb(samples, static_cast<size_t>(inNumberFrames));
    return noErr;
}

OSStatus VpioUnit::render_callback(void* inRefCon,
                                  AudioUnitRenderActionFlags* /* ioActionFlags */,
                                  const AudioTimeStamp* /* inTimeStamp */,
                                  UInt32 /* inBusNumber */,
                                  UInt32 inNumberFrames,
                                  AudioBufferList* ioData) {
    auto* self = static_cast<VpioUnit*>(inRefCon);
    int16_t* out = static_cast<int16_t*>(ioData->mBuffers[0].mData);
    const size_t wanted = static_cast<size_t>(inNumberFrames);

    size_t got = 0;
    if (self->render_source_) {
        got = self->render_source_(out, wanted);
    } else {
        got = self->ring_.read(out, wanted);
    }
    if (got < wanted) {
        std::memset(out + got, 0, (wanted - got) * sizeof(int16_t));
    }
    return noErr;
}

// --- Adapters ---

bool VpioCaptureAdapter::initialize(const char* /*device_id*/) {
    // The unit is fully initialized before adapters are built; verify live.
    return unit_ && unit_->initialized();
}

bool VpioCaptureAdapter::start(Callback callback) {
    if (!unit_) return false;
    return unit_->start_capture(std::move(callback));
}

void VpioCaptureAdapter::stop() {
    if (unit_) unit_->stop_capture();
}

int VpioCaptureAdapter::input_latency_ms() const {
    return unit_ ? unit_->input_latency_ms() : 0;
}

bool VpioCaptureAdapter::provides_echo_cancellation() const {
    return unit_ && unit_->initialized();
}

bool VpioPlaybackAdapter::initialize(const char* /*device_id*/) {
    return unit_ && unit_->initialized();
}

bool VpioPlaybackAdapter::start() {
    if (!unit_) return false;
    return unit_->start_playback();
}

void VpioPlaybackAdapter::stop() {
    if (unit_) unit_->stop_playback();
}

size_t VpioPlaybackAdapter::feed(const int16_t* samples, size_t count) {
    if (!unit_) return 0;
    return unit_->feed(samples, count);
}

void VpioPlaybackAdapter::set_render_source(RenderSourceFn fn) {
    // Forward to the unit: the base-class member would leave the unit's
    // render callback pulling silence (this exact bug stalled all VPIO
    // playout — clips and remote voices alike).
    render_source_ = std::move(fn);
    if (unit_) unit_->set_render_source(render_source_);
}

void VpioPlaybackAdapter::set_input_channels(uint32_t channels) {
    if (channels != 1) {
        MELLO_LOG_WARN("vpio", "voice duplex is mono-only, ignoring channels=%u", channels);
    }
}

int VpioPlaybackAdapter::output_latency_ms() const {
    return unit_ ? unit_->output_latency_ms() : 0;
}

}  // namespace mello::audio
#endif
