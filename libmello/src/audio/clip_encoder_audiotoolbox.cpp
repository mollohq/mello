#include "clip_encoder.hpp"
#include "../util/log.hpp"

#include <AudioToolbox/AudioToolbox.h>
#include <CoreFoundation/CoreFoundation.h>
#include <algorithm>

namespace mello::audio {

namespace {

struct CfUrl {
    CFURLRef ref = nullptr;

    explicit CfUrl(const std::string& path) {
        ref = CFURLCreateFromFileSystemRepresentation(
            kCFAllocatorDefault,
            reinterpret_cast<const UInt8*>(path.c_str()),
            static_cast<CFIndex>(path.size()),
            false);
    }

    ~CfUrl() { if (ref) CFRelease(ref); }
    explicit operator bool() const { return ref != nullptr; }

    CfUrl(const CfUrl&) = delete;
    CfUrl& operator=(const CfUrl&) = delete;
};

} // anonymous namespace

bool encode_wav_to_mp4(const std::string& wav_path,
                       const std::string& mp4_path,
                       int bitrate) {
    std::vector<int16_t> pcm;
    uint32_t sample_rate = 0;
    uint16_t channels = 0;
    if (!detail::read_wav_pcm(wav_path, pcm, sample_rate, channels)) {
        MELLO_LOG_ERROR("clip_encoder", "failed to read WAV: %s", wav_path.c_str());
        return false;
    }
    if (pcm.empty()) {
        MELLO_LOG_ERROR("clip_encoder", "WAV file is empty: %s", wav_path.c_str());
        return false;
    }

    CfUrl url(mp4_path);
    if (!url) {
        MELLO_LOG_ERROR("clip_encoder", "invalid output path: %s", mp4_path.c_str());
        return false;
    }

    AudioStreamBasicDescription pcm_desc{};
    pcm_desc.mSampleRate       = sample_rate;
    pcm_desc.mFormatID         = kAudioFormatLinearPCM;
    pcm_desc.mFormatFlags      = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked;
    pcm_desc.mBitsPerChannel   = 16;
    pcm_desc.mChannelsPerFrame = channels;
    pcm_desc.mFramesPerPacket  = 1;
    pcm_desc.mBytesPerFrame    = channels * 2;
    pcm_desc.mBytesPerPacket   = channels * 2;

    AudioStreamBasicDescription aac_desc{};
    aac_desc.mSampleRate       = sample_rate;
    aac_desc.mFormatID         = kAudioFormatMPEG4AAC;
    aac_desc.mChannelsPerFrame = channels;

    ExtAudioFileRef out_file = nullptr;
    OSStatus status = ExtAudioFileCreateWithURL(
        url.ref,
        kAudioFileM4AType,
        &aac_desc,
        nullptr,
        kAudioFileFlags_EraseFile,
        &out_file);
    if (status != noErr) {
        MELLO_LOG_ERROR("clip_encoder", "ExtAudioFileCreateWithURL failed: %d", (int)status);
        return false;
    }

    status = ExtAudioFileSetProperty(out_file,
        kExtAudioFileProperty_ClientDataFormat,
        sizeof(pcm_desc), &pcm_desc);
    if (status != noErr) {
        MELLO_LOG_ERROR("clip_encoder", "set client format failed: %d", (int)status);
        ExtAudioFileDispose(out_file);
        return false;
    }

    AudioConverterRef converter = nullptr;
    UInt32 conv_size = sizeof(converter);
    status = ExtAudioFileGetProperty(out_file,
        kExtAudioFileProperty_AudioConverter, &conv_size, &converter);
    if (status == noErr && converter) {
        UInt32 br = static_cast<UInt32>(bitrate);
        AudioConverterSetProperty(converter,
            kAudioConverterEncodeBitRate, sizeof(br), &br);
    }

    const UInt32 frames_per_chunk = 1024;
    UInt32 total_frames = static_cast<UInt32>(pcm.size() / channels);
    UInt32 frames_written = 0;

    while (frames_written < total_frames) {
        UInt32 chunk = std::min(frames_per_chunk, total_frames - frames_written);

        AudioBufferList buf_list;
        buf_list.mNumberBuffers = 1;
        buf_list.mBuffers[0].mNumberChannels = channels;
        buf_list.mBuffers[0].mDataByteSize   = chunk * channels * sizeof(int16_t);
        buf_list.mBuffers[0].mData           = const_cast<int16_t*>(
            pcm.data() + frames_written * channels);

        status = ExtAudioFileWrite(out_file, chunk, &buf_list);
        if (status != noErr) {
            MELLO_LOG_ERROR("clip_encoder", "ExtAudioFileWrite failed at frame %u: %d",
                            frames_written, (int)status);
            ExtAudioFileDispose(out_file);
            return false;
        }
        frames_written += chunk;
    }

    ExtAudioFileDispose(out_file);

    float duration_s = static_cast<float>(pcm.size()) / (sample_rate * channels);
    MELLO_LOG_INFO("clip_encoder", "encoded %s -> %s (%.1fs, %dkbps)",
                   wav_path.c_str(), mp4_path.c_str(), duration_s, bitrate / 1000);
    return true;
}

std::vector<int16_t> decode_mp4_to_pcm(const std::string& mp4_path) {
    CfUrl url(mp4_path);
    if (!url) {
        MELLO_LOG_ERROR("clip_encoder", "invalid path: %s", mp4_path.c_str());
        return {};
    }

    ExtAudioFileRef in_file = nullptr;
    OSStatus status = ExtAudioFileOpenURL(url.ref, &in_file);
    if (status != noErr) {
        MELLO_LOG_ERROR("clip_encoder", "ExtAudioFileOpenURL failed: %d", (int)status);
        return {};
    }

    AudioStreamBasicDescription pcm_desc{};
    pcm_desc.mSampleRate       = 48000;
    pcm_desc.mFormatID         = kAudioFormatLinearPCM;
    pcm_desc.mFormatFlags      = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked;
    pcm_desc.mBitsPerChannel   = 16;
    pcm_desc.mChannelsPerFrame = 1;
    pcm_desc.mFramesPerPacket  = 1;
    pcm_desc.mBytesPerFrame    = 2;
    pcm_desc.mBytesPerPacket   = 2;

    status = ExtAudioFileSetProperty(in_file,
        kExtAudioFileProperty_ClientDataFormat,
        sizeof(pcm_desc), &pcm_desc);
    if (status != noErr) {
        MELLO_LOG_ERROR("clip_encoder", "set client format failed: %d", (int)status);
        ExtAudioFileDispose(in_file);
        return {};
    }

    std::vector<int16_t> pcm;
    pcm.reserve(48000 * 30);

    const UInt32 frames_per_read = 4096;
    std::vector<int16_t> buf(frames_per_read);

    for (;;) {
        UInt32 frames = frames_per_read;
        AudioBufferList buf_list;
        buf_list.mNumberBuffers = 1;
        buf_list.mBuffers[0].mNumberChannels = 1;
        buf_list.mBuffers[0].mDataByteSize   = frames * sizeof(int16_t);
        buf_list.mBuffers[0].mData           = buf.data();

        status = ExtAudioFileRead(in_file, &frames, &buf_list);
        if (status != noErr || frames == 0) break;

        pcm.insert(pcm.end(), buf.begin(), buf.begin() + frames);
    }

    ExtAudioFileDispose(in_file);

    MELLO_LOG_INFO("clip_encoder", "decoded %s -> %zu samples (%.1fs)",
                   mp4_path.c_str(), pcm.size(),
                   static_cast<float>(pcm.size()) / 48000.0f);
    return pcm;
}

} // namespace mello::audio
