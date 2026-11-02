#include "mp4_read.hpp"

#ifdef _WIN32
#include "../clip/h264_annex_b.hpp"
#include "../util/log.hpp"

#include <mfapi.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <wrl/client.h>

namespace mello::replay {

using Microsoft::WRL::ComPtr;

namespace {

constexpr const char* TAG = "replay/read";

/// Media Foundation for one read, on the caller's thread.
struct MfScope {
    HRESULT co = E_FAIL;
    HRESULT mf = E_FAIL;
    MfScope() {
        co = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        mf = MFStartup(MF_VERSION);
    }
    ~MfScope() {
        if (SUCCEEDED(mf)) MFShutdown();
        if (SUCCEEDED(co)) CoUninitialize();
    }
};

ComPtr<IMFSourceReader> open_reader(const std::filesystem::path& mp4) {
    ComPtr<IMFSourceReader> reader;
    const HRESULT hr = MFCreateSourceReaderFromURL(mp4.wstring().c_str(), nullptr, &reader);
    if (FAILED(hr)) {
        MELLO_LOG_WARN(TAG, "%s does not open: hr=0x%08X", mp4.u8string().c_str(),
                       static_cast<unsigned>(hr));
        return nullptr;
    }
    return reader;
}

bool sample_bytes(IMFSample* sample, std::vector<uint8_t>& out) {
    ComPtr<IMFMediaBuffer> buffer;
    if (FAILED(sample->ConvertToContiguousBuffer(&buffer))) return false;
    BYTE* data = nullptr;
    DWORD length = 0;
    if (FAILED(buffer->Lock(&data, nullptr, &length))) return false;
    out.assign(data, data + length);
    buffer->Unlock();
    return true;
}

}  // namespace

bool read_mp4_video(const std::filesystem::path& mp4,
                    const std::function<bool(const Mp4VideoUnit&)>& on_unit, uint32_t* width,
                    uint32_t* height) {
    MfScope mf;
    if (FAILED(mf.mf)) return false;
    auto reader = open_reader(mp4);
    if (!reader) return false;
    const DWORD video = static_cast<DWORD>(MF_SOURCE_READER_FIRST_VIDEO_STREAM);
    ComPtr<IMFMediaType> native;
    if (FAILED(reader->GetNativeMediaType(video, 0, &native))) return false;
    UINT32 w = 0, h = 0;
    MFGetAttributeSize(native.Get(), MF_MT_FRAME_SIZE, &w, &h);
    if (width) *width = w;
    if (height) *height = h;
    std::vector<uint8_t> header;
    UINT32 blob = 0;
    if (SUCCEEDED(native->GetBlobSize(MF_MT_MPEG_SEQUENCE_HEADER, &blob)) && blob > 0) {
        header.resize(blob);
        native->GetBlob(MF_MT_MPEG_SEQUENCE_HEADER, header.data(), blob, nullptr);
        header = clip::to_annex_b(std::move(header));
    }
    reader->SetStreamSelection(static_cast<DWORD>(MF_SOURCE_READER_ALL_STREAMS), FALSE);
    reader->SetStreamSelection(video, TRUE);

    bool first = true;
    for (;;) {
        DWORD flags = 0;
        LONGLONG time = 0;
        ComPtr<IMFSample> sample;
        if (FAILED(reader->ReadSample(video, 0, nullptr, &flags, &time, &sample))) return false;
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM) return true;
        if (!sample) continue;
        Mp4VideoUnit unit;
        if (!sample_bytes(sample.Get(), unit.annex_b)) return false;
        unit.annex_b = clip::to_annex_b(std::move(unit.annex_b));
        unit.keyframe = MFGetAttributeUINT32(sample.Get(), MFSampleExtension_CleanPoint, FALSE) != 0;
        unit.time_us = time / 10;
        if (first && unit.keyframe && !header.empty()) {
            unit.annex_b.insert(unit.annex_b.begin(), header.begin(), header.end());
        }
        first = false;
        if (!on_unit(unit)) return true;
    }
}

bool read_mp4_audio(const std::filesystem::path& mp4,
                    const std::function<bool(const int16_t* stereo, size_t frames, int64_t time_us)>& on_pcm) {
    MfScope mf;
    if (FAILED(mf.mf)) return false;
    auto reader = open_reader(mp4);
    if (!reader) return false;
    const DWORD audio = static_cast<DWORD>(MF_SOURCE_READER_FIRST_AUDIO_STREAM);
    ComPtr<IMFMediaType> pcm;
    MFCreateMediaType(&pcm);
    pcm->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
    pcm->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_PCM);
    pcm->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
    pcm->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000);
    pcm->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, 2);
    if (FAILED(reader->SetCurrentMediaType(audio, nullptr, pcm.Get()))) return false;
    reader->SetStreamSelection(static_cast<DWORD>(MF_SOURCE_READER_ALL_STREAMS), FALSE);
    reader->SetStreamSelection(audio, TRUE);
    std::vector<uint8_t> bytes;
    for (;;) {
        DWORD flags = 0;
        LONGLONG time = 0;
        ComPtr<IMFSample> sample;
        if (FAILED(reader->ReadSample(audio, 0, nullptr, &flags, &time, &sample))) return false;
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM) return true;
        if (!sample) continue;
        if (!sample_bytes(sample.Get(), bytes)) return false;
        const size_t frames = bytes.size() / (2 * sizeof(int16_t));
        if (frames > 0 &&
            !on_pcm(reinterpret_cast<const int16_t*>(bytes.data()), frames, time / 10)) {
            return true;
        }
    }
}

}  // namespace mello::replay

#else

namespace mello::replay {

bool read_mp4_video(const std::filesystem::path&, const std::function<bool(const Mp4VideoUnit&)>&,
                    uint32_t*, uint32_t*) {
    return false;
}

bool read_mp4_audio(const std::filesystem::path&,
                    const std::function<bool(const int16_t*, size_t, int64_t)>&) {
    return false;
}

}  // namespace mello::replay

#endif
