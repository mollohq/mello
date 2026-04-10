#include "clip_encoder.hpp"
#include "../util/log.hpp"

#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <mferror.h>
#include <wrl/client.h>
#include <algorithm>
#include <cstring>

using Microsoft::WRL::ComPtr;

namespace mello::audio {

namespace {

struct MfSession {
    bool must_uninit_com = false;
    bool ok = false;

    MfSession() {
        HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        must_uninit_com = (hr == S_OK);
        ok = SUCCEEDED(MFStartup(MF_VERSION));
    }

    ~MfSession() {
        if (ok) MFShutdown();
        if (must_uninit_com) CoUninitialize();
    }

    explicit operator bool() const { return ok; }
};

std::wstring utf8_to_wide(const std::string& s) {
    if (s.empty()) return {};
    int len = MultiByteToWideChar(CP_UTF8, 0, s.c_str(), -1, nullptr, 0);
    std::wstring w(len, 0);
    MultiByteToWideChar(CP_UTF8, 0, s.c_str(), -1, w.data(), len);
    return w;
}

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

    MfSession mf;
    if (!mf) {
        MELLO_LOG_ERROR("clip_encoder", "MFStartup failed");
        return false;
    }

    auto wide_path = utf8_to_wide(mp4_path);

    ComPtr<IMFSinkWriter> writer;
    HRESULT hr = MFCreateSinkWriterFromURL(wide_path.c_str(), nullptr, nullptr, &writer);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("clip_encoder", "MFCreateSinkWriterFromURL failed: 0x%08lx", hr);
        return false;
    }

    // Output stream: AAC-LC
    ComPtr<IMFMediaType> out_type;
    MFCreateMediaType(&out_type);
    out_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
    out_type->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_AAC);
    out_type->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
    out_type->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate);
    out_type->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, channels);
    out_type->SetUINT32(MF_MT_AUDIO_AVG_BYTES_PER_SECOND, static_cast<UINT32>(bitrate / 8));

    DWORD stream_idx = 0;
    hr = writer->AddStream(out_type.Get(), &stream_idx);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("clip_encoder", "AddStream(AAC) failed: 0x%08lx", hr);
        return false;
    }

    // Input stream: PCM
    ComPtr<IMFMediaType> in_type;
    MFCreateMediaType(&in_type);
    in_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
    in_type->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_PCM);
    in_type->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
    in_type->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate);
    in_type->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, channels);

    hr = writer->SetInputMediaType(stream_idx, in_type.Get(), nullptr);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("clip_encoder", "SetInputMediaType(PCM) failed: 0x%08lx", hr);
        return false;
    }

    hr = writer->BeginWriting();
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("clip_encoder", "BeginWriting failed: 0x%08lx", hr);
        return false;
    }

    const size_t chunk_samples = 1024 * channels;
    const LONGLONG ticks_per_sample = 10000000LL / sample_rate; // 100-ns units
    LONGLONG timestamp = 0;

    for (size_t offset = 0; offset < pcm.size(); offset += chunk_samples) {
        size_t count = (std::min)(chunk_samples, pcm.size() - offset);
        DWORD byte_count = static_cast<DWORD>(count * sizeof(int16_t));

        ComPtr<IMFMediaBuffer> buffer;
        hr = MFCreateMemoryBuffer(byte_count, &buffer);
        if (FAILED(hr)) break;

        BYTE* buf_data = nullptr;
        buffer->Lock(&buf_data, nullptr, nullptr);
        memcpy(buf_data, pcm.data() + offset, byte_count);
        buffer->Unlock();
        buffer->SetCurrentLength(byte_count);

        ComPtr<IMFSample> sample;
        MFCreateSample(&sample);
        sample->AddBuffer(buffer.Get());

        LONGLONG duration = static_cast<LONGLONG>(count / channels) * ticks_per_sample;
        sample->SetSampleTime(timestamp);
        sample->SetSampleDuration(duration);

        hr = writer->WriteSample(stream_idx, sample.Get());
        if (FAILED(hr)) {
            MELLO_LOG_ERROR("clip_encoder", "WriteSample failed at offset %zu: 0x%08lx",
                            offset, hr);
            return false;
        }
        timestamp += duration;
    }

    hr = writer->Finalize();
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("clip_encoder", "Finalize failed: 0x%08lx", hr);
        return false;
    }

    float duration_s = static_cast<float>(pcm.size()) / (sample_rate * channels);
    MELLO_LOG_INFO("clip_encoder", "encoded %s -> %s (%.1fs, %dkbps)",
                   wav_path.c_str(), mp4_path.c_str(), duration_s, bitrate / 1000);
    return true;
}

std::vector<int16_t> decode_mp4_to_pcm(const std::string& mp4_path) {
    MfSession mf;
    if (!mf) {
        MELLO_LOG_ERROR("clip_encoder", "MFStartup failed for decode");
        return {};
    }

    auto wide_path = utf8_to_wide(mp4_path);

    ComPtr<IMFSourceReader> reader;
    HRESULT hr = MFCreateSourceReaderFromURL(wide_path.c_str(), nullptr, &reader);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("clip_encoder", "MFCreateSourceReaderFromURL failed: 0x%08lx", hr);
        return {};
    }

    ComPtr<IMFMediaType> pcm_type;
    MFCreateMediaType(&pcm_type);
    pcm_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
    pcm_type->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_PCM);
    pcm_type->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
    pcm_type->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000);
    pcm_type->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, 1);

    hr = reader->SetCurrentMediaType(
        static_cast<DWORD>(MF_SOURCE_READER_FIRST_AUDIO_STREAM),
        nullptr, pcm_type.Get());
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("clip_encoder", "SetCurrentMediaType(PCM) failed: 0x%08lx", hr);
        return {};
    }

    std::vector<int16_t> pcm;
    pcm.reserve(48000 * 30);

    for (;;) {
        DWORD flags = 0;
        ComPtr<IMFSample> sample;
        hr = reader->ReadSample(
            static_cast<DWORD>(MF_SOURCE_READER_FIRST_AUDIO_STREAM),
            0, nullptr, &flags, nullptr, &sample);

        if (FAILED(hr) || (flags & MF_SOURCE_READERF_ENDOFSTREAM)) break;
        if (!sample) continue;

        ComPtr<IMFMediaBuffer> buffer;
        sample->ConvertToContiguousBuffer(&buffer);

        BYTE* data = nullptr;
        DWORD length = 0;
        buffer->Lock(&data, nullptr, &length);

        size_t count = length / sizeof(int16_t);
        size_t prev = pcm.size();
        pcm.resize(prev + count);
        memcpy(pcm.data() + prev, data, length);

        buffer->Unlock();
    }

    MELLO_LOG_INFO("clip_encoder", "decoded %s -> %zu samples (%.1fs)",
                   mp4_path.c_str(), pcm.size(),
                   static_cast<float>(pcm.size()) / 48000.0f);
    return pcm;
}

} // namespace mello::audio
