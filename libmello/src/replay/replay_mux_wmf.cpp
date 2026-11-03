#include "replay_mux.hpp"
#include "h264_nal.hpp"
#include "mp4_faststart.hpp"
#include "../util/log.hpp"

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <codecapi.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <wincodec.h>
#include <wrl/client.h>
#include <algorithm>
#include <cstring>
#include <system_error>

using Microsoft::WRL::ComPtr;

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/mux";
constexpr uint32_t kAacBlockFrames = 1024;

// COM (STA, as every COM thread in libmello, 03-LIBMELLO 4.1) and Media
// Foundation for one call. A thread that the host already initialized keeps
// its apartment.
struct MfSession {
    bool must_uninit_com = false;
    bool ok = false;
    MfSession() {
        const HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
        must_uninit_com = hr == S_OK;
        ok = SUCCEEDED(MFStartup(MF_VERSION));
    }
    ~MfSession() {
        if (ok) MFShutdown();
        if (must_uninit_com) CoUninitialize();
    }
};

LONGLONG hns(uint64_t us) { return static_cast<LONGLONG>(us) * 10; }

// The SPS and PPS of an IDR access unit, each with a 4-byte start code: the
// sequence header of the H.264 media type.
std::vector<uint8_t> sequence_header(const std::vector<uint8_t>& idr) {
    std::vector<uint8_t> out;
    static const uint8_t kStart[] = {0, 0, 0, 1};
    h264::for_each_nal(idr.data(), idr.size(), [&](uint8_t type, const uint8_t* nal, size_t size) {
        if (type == h264::kNalSps || type == h264::kNalPps) {
            out.insert(out.end(), kStart, kStart + 4);
            out.insert(out.end(), nal, nal + size);
        }
    });
    return out;
}

ComPtr<IMFSample> make_sample(const void* data, size_t size, LONGLONG time, LONGLONG duration) {
    ComPtr<IMFMediaBuffer> buffer;
    if (FAILED(MFCreateMemoryBuffer(static_cast<DWORD>(size), &buffer))) return nullptr;
    BYTE* dst = nullptr;
    if (FAILED(buffer->Lock(&dst, nullptr, nullptr))) return nullptr;
    std::memcpy(dst, data, size);
    buffer->Unlock();
    buffer->SetCurrentLength(static_cast<DWORD>(size));
    ComPtr<IMFSample> sample;
    if (FAILED(MFCreateSample(&sample))) return nullptr;
    sample->AddBuffer(buffer.Get());
    sample->SetSampleTime(time);
    sample->SetSampleDuration(duration);
    return sample;
}

} // namespace

bool write_clip_mp4(const ClipMuxInput& in, const std::filesystem::path& path,
                    ClipMuxResult* result) {
    if (in.units.empty() || !in.read_unit || in.width == 0 || in.height == 0) {
        MELLO_LOG_ERROR(TAG, "mux: no video");
        return false;
    }
    if (!in.units.front().keyframe) {
        MELLO_LOG_ERROR(TAG, "mux: the first unit is not a keyframe");
        return false;
    }
    std::vector<uint8_t> bytes;
    if (!in.read_unit(in.units.front(), bytes)) {
        MELLO_LOG_ERROR(TAG, "mux: cannot read the first unit");
        return false;
    }
    const std::vector<uint8_t> seq_header = sequence_header(bytes);
    if (seq_header.empty()) {
        MELLO_LOG_ERROR(TAG, "mux: the first unit has no SPS and PPS");
        return false;
    }

    // The video times: each frame at its capture time from the first one,
    // lasting until the next. A frame time that does not move forward (it
    // cannot, the ring keeps capture order) gets 0.1 ms.
    const uint64_t t0 = in.units.front().capture_us;
    const size_t n = in.units.size();
    std::vector<LONGLONG> v_time(n), v_dur(n);
    uint64_t total_bytes = 0;
    for (size_t i = 0; i < n; ++i) {
        const uint64_t at = in.units[i].capture_us >= t0 ? in.units[i].capture_us - t0 : 0;
        v_time[i] = hns(at);
        if (i > 0 && v_time[i] <= v_time[i - 1]) v_time[i] = v_time[i - 1] + 1000;
        total_bytes += in.units[i].size;
    }
    const LONGLONG nominal = 10'000'000 / std::max<uint32_t>(in.fps, 1);
    const LONGLONG clip_end = in.end_us > t0 ? hns(in.end_us - t0) : 0;
    for (size_t i = 0; i < n; ++i) {
        const LONGLONG next = i + 1 < n ? v_time[i + 1] : clip_end;
        v_dur[i] = next > v_time[i] ? next - v_time[i] : nominal;
    }
    const LONGLONG video_end = v_time[n - 1] + v_dur[n - 1];

    MfSession mf;
    if (!mf.ok) {
        MELLO_LOG_ERROR(TAG, "mux: MFStartup failed");
        return false;
    }

    // Media Foundation writes the moov box last. Its own moov-first mode
    // (MF_MPEG4SINK_MOOV_BEFORE_MDAT) wrote chunk offsets that point before
    // the media data on Windows 11 (2026-11-02), so mp4_faststart moves moov
    // to the front after Finalize.
    ComPtr<IMFAttributes> attrs;
    MFCreateAttributes(&attrs, 2);
    attrs->SetGUID(MF_TRANSCODE_CONTAINERTYPE, MFTranscodeContainerType_MPEG4);
    attrs->SetUINT32(MF_SINK_WRITER_DISABLE_THROTTLING, TRUE);

    std::error_code ec;
    std::filesystem::path part = path;
    part += ".part";
    std::filesystem::remove(part, ec);
    ComPtr<IMFSinkWriter> writer;
    HRESULT hr = MFCreateSinkWriterFromURL(part.wstring().c_str(), nullptr, attrs.Get(), &writer);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "mux: MFCreateSinkWriterFromURL failed: 0x%08lx", hr);
        return false;
    }
    // A failed mux leaves no part file.
    struct PartFile {
        const std::filesystem::path& p;
        ~PartFile() {
            std::error_code e;
            std::filesystem::remove(p, e);
        }
    } part_guard{part};

    // H.264 in, H.264 out: the sink writer passes the units through.
    ComPtr<IMFMediaType> vtype;
    MFCreateMediaType(&vtype);
    vtype->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    vtype->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_H264);
    MFSetAttributeSize(vtype.Get(), MF_MT_FRAME_SIZE, in.width, in.height);
    MFSetAttributeRatio(vtype.Get(), MF_MT_FRAME_RATE, std::max<uint32_t>(in.fps, 1), 1);
    MFSetAttributeRatio(vtype.Get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
    vtype->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    vtype->SetUINT32(MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High);
    const uint64_t video_us = std::max<uint64_t>(static_cast<uint64_t>(video_end / 10), 1);
    vtype->SetUINT32(MF_MT_AVG_BITRATE,
                     static_cast<UINT32>(std::min<uint64_t>(total_bytes * 8 * 1'000'000 / video_us,
                                                            UINT32_MAX)));
    vtype->SetBlob(MF_MT_MPEG_SEQUENCE_HEADER, seq_header.data(),
                   static_cast<UINT32>(seq_header.size()));
    DWORD vstream = 0;
    hr = writer->AddStream(vtype.Get(), &vstream);
    if (SUCCEEDED(hr)) hr = writer->SetInputMediaType(vstream, vtype.Get(), nullptr);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "mux: the H.264 stream was refused: 0x%08lx", hr);
        return false;
    }

    const bool has_audio = !in.audio.empty();
    DWORD astream = 0;
    if (has_audio) {
        ComPtr<IMFMediaType> out_type;
        MFCreateMediaType(&out_type);
        out_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
        out_type->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_AAC);
        out_type->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
        out_type->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000);
        out_type->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, 2);
        out_type->SetUINT32(MF_MT_AUDIO_AVG_BYTES_PER_SECOND, in.audio_bitrate / 8);
        ComPtr<IMFMediaType> pcm_type;
        MFCreateMediaType(&pcm_type);
        pcm_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
        pcm_type->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_PCM);
        pcm_type->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
        pcm_type->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000);
        pcm_type->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, 2);
        pcm_type->SetUINT32(MF_MT_AUDIO_BLOCK_ALIGNMENT, 4);
        pcm_type->SetUINT32(MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 48000 * 4);
        hr = writer->AddStream(out_type.Get(), &astream);
        if (SUCCEEDED(hr)) hr = writer->SetInputMediaType(astream, pcm_type.Get(), nullptr);
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "mux: the AAC stream was refused: 0x%08lx", hr);
            return false;
        }
    }

    hr = writer->BeginWriting();
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "mux: BeginWriting failed: 0x%08lx", hr);
        return false;
    }

    // Video and audio in time order, so the sink interleaves them.
    const size_t audio_frames = in.audio.size() / 2;
    size_t vi = 0;
    size_t af = 0;
    while (vi < n || (has_audio && af < audio_frames)) {
        const LONGLONG a_time = static_cast<LONGLONG>(af) * 10'000'000 / 48000;
        const bool video_next =
            vi < n && (!has_audio || af >= audio_frames || v_time[vi] <= a_time);
        if (video_next) {
            const AccessUnitRef& unit = in.units[vi];
            if (vi > 0 && !in.read_unit(unit, bytes)) {
                MELLO_LOG_ERROR(TAG, "mux: cannot read unit %zu", vi);
                return false;
            }
            ComPtr<IMFSample> sample = make_sample(bytes.data(), bytes.size(), v_time[vi], v_dur[vi]);
            if (!sample) return false;
            if (unit.keyframe) sample->SetUINT32(MFSampleExtension_CleanPoint, TRUE);
            hr = writer->WriteSample(vstream, sample.Get());
            if (FAILED(hr)) {
                MELLO_LOG_ERROR(TAG, "mux: WriteSample(video %zu) failed: 0x%08lx", vi, hr);
                return false;
            }
            ++vi;
        } else {
            const size_t frames = std::min<size_t>(kAacBlockFrames, audio_frames - af);
            const LONGLONG duration = static_cast<LONGLONG>(frames) * 10'000'000 / 48000;
            ComPtr<IMFSample> sample = make_sample(in.audio.data() + af * 2,
                                                   frames * 2 * sizeof(int16_t), a_time, duration);
            if (!sample) return false;
            hr = writer->WriteSample(astream, sample.Get());
            if (FAILED(hr)) {
                MELLO_LOG_ERROR(TAG, "mux: WriteSample(audio) failed: 0x%08lx", hr);
                return false;
            }
            af += frames;
        }
    }

    hr = writer->Finalize();
    writer.Reset();
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "mux: Finalize failed: 0x%08lx", hr);
        return false;
    }
    // Web and phone playback start before the download ends (CLIPS.md 15.5).
    if (!mp4_faststart(part, path)) {
        MELLO_LOG_ERROR(TAG, "mux: moving moov to the front failed");
        std::filesystem::remove(path, ec);
        return false;
    }

    const uint64_t file_bytes = std::filesystem::file_size(path, ec);
    if (result) {
        result->duration_us = static_cast<uint64_t>(video_end / 10);
        result->video_frames = static_cast<uint32_t>(n);
        result->bytes = ec ? 0 : file_bytes;
    }
    MELLO_LOG_INFO(TAG, "mux: %s, %zu frames, %.2f s, %llu bytes, audio %s",
                   path.string().c_str(), n, static_cast<double>(video_end) / 1e7,
                   static_cast<unsigned long long>(ec ? 0 : file_bytes), has_audio ? "AAC" : "none");
    return true;
}

bool write_poster_jpeg(const uint8_t* bgra, uint32_t width, uint32_t height, uint32_t stride,
                       const std::filesystem::path& path) {
    if (!bgra || width == 0 || height == 0) return false;
    const HRESULT co = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    const bool must_uninit = co == S_OK;
    bool ok = false;
    {
        ComPtr<IWICImagingFactory> factory;
        ComPtr<IWICBitmap> bitmap;
        ComPtr<IWICStream> stream;
        ComPtr<IWICBitmapEncoder> encoder;
        ComPtr<IWICBitmapFrameEncode> frame;
        ComPtr<IPropertyBag2> props;
        HRESULT hr = CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER,
                                      IID_PPV_ARGS(&factory));
        if (SUCCEEDED(hr)) {
            hr = factory->CreateBitmapFromMemory(width, height, GUID_WICPixelFormat32bppBGR, stride,
                                                 stride * height, const_cast<BYTE*>(bgra), &bitmap);
        }
        if (SUCCEEDED(hr)) hr = factory->CreateStream(&stream);
        if (SUCCEEDED(hr)) hr = stream->InitializeFromFilename(path.wstring().c_str(), GENERIC_WRITE);
        if (SUCCEEDED(hr)) hr = factory->CreateEncoder(GUID_ContainerFormatJpeg, nullptr, &encoder);
        if (SUCCEEDED(hr)) hr = encoder->Initialize(stream.Get(), WICBitmapEncoderNoCache);
        if (SUCCEEDED(hr)) hr = encoder->CreateNewFrame(&frame, &props);
        if (SUCCEEDED(hr)) {
            PROPBAG2 option{};
            option.pstrName = const_cast<LPOLESTR>(L"ImageQuality");
            VARIANT value;
            VariantInit(&value);
            value.vt = VT_R4;
            value.fltVal = 0.85f;
            props->Write(1, &option, &value);
            hr = frame->Initialize(props.Get());
        }
        if (SUCCEEDED(hr)) hr = frame->SetSize(width, height);
        // The JPEG encoder takes 24-bit BGR; WriteSource converts.
        if (SUCCEEDED(hr)) hr = frame->WriteSource(bitmap.Get(), nullptr);
        if (SUCCEEDED(hr)) hr = frame->Commit();
        if (SUCCEEDED(hr)) hr = encoder->Commit();
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "poster: JPEG %ux%u failed: 0x%08lx", width, height, hr);
        }
        ok = SUCCEEDED(hr);
    }
    if (must_uninit) CoUninitialize();
    return ok;
}

bool write_poster_from_mp4(const std::filesystem::path& mp4, uint64_t time_us,
                           const std::filesystem::path& jpeg) {
    MfSession mf;
    if (!mf.ok) return false;

    ComPtr<IMFAttributes> attrs;
    MFCreateAttributes(&attrs, 1);
    // The source reader converts the decoded frame to RGB32.
    attrs->SetUINT32(MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, TRUE);
    ComPtr<IMFSourceReader> reader;
    HRESULT hr = MFCreateSourceReaderFromURL(mp4.wstring().c_str(), attrs.Get(), &reader);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "poster: cannot open %s: 0x%08lx", mp4.string().c_str(), hr);
        return false;
    }
    const DWORD video = static_cast<DWORD>(MF_SOURCE_READER_FIRST_VIDEO_STREAM);
    reader->SetStreamSelection(static_cast<DWORD>(MF_SOURCE_READER_ALL_STREAMS), FALSE);
    reader->SetStreamSelection(video, TRUE);
    ComPtr<IMFMediaType> rgb;
    MFCreateMediaType(&rgb);
    rgb->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    rgb->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_RGB32);
    hr = reader->SetCurrentMediaType(video, nullptr, rgb.Get());
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "poster: no RGB32 output: 0x%08lx", hr);
        return false;
    }
    PROPVARIANT at;
    PropVariantInit(&at);
    at.vt = VT_I8;
    at.hVal.QuadPart = hns(time_us);
    reader->SetCurrentPosition(GUID_NULL, at);

    ComPtr<IMFMediaType> current;
    reader->GetCurrentMediaType(video, &current);
    UINT32 width = 0, height = 0;
    MFGetAttributeSize(current.Get(), MF_MT_FRAME_SIZE, &width, &height);
    // A decoder pads the height to whole macroblocks (1088 for 1080). The
    // display aperture is the picture.
    MFVideoArea area{};
    if (SUCCEEDED(current->GetBlob(MF_MT_MINIMUM_DISPLAY_APERTURE, reinterpret_cast<UINT8*>(&area),
                                   sizeof(area), nullptr)) &&
        area.Area.cx > 0 && area.Area.cy > 0) {
        width = static_cast<UINT32>(area.Area.cx);
        height = static_cast<UINT32>(area.Area.cy);
    }

    for (int reads = 0; reads < 600; ++reads) {
        DWORD flags = 0;
        LONGLONG timestamp = 0;
        ComPtr<IMFSample> sample;
        hr = reader->ReadSample(video, 0, nullptr, &flags, &timestamp, &sample);
        if (FAILED(hr) || (flags & MF_SOURCE_READERF_ENDOFSTREAM)) break;
        if (!sample || timestamp + 1000 < hns(time_us)) continue;

        ComPtr<IMFMediaBuffer> buffer;
        sample->ConvertToContiguousBuffer(&buffer);
        ComPtr<IMF2DBuffer> buffer2d;
        BYTE* scan0 = nullptr;
        LONG pitch = 0;
        bool locked = false;
        if (SUCCEEDED(buffer.As(&buffer2d)) && SUCCEEDED(buffer2d->Lock2D(&scan0, &pitch))) {
            locked = true;
        }
        std::vector<uint8_t> top_down(static_cast<size_t>(width) * height * 4);
        if (locked) {
            for (UINT32 y = 0; y < height; ++y) {
                std::memcpy(top_down.data() + static_cast<size_t>(y) * width * 4,
                            scan0 + static_cast<ptrdiff_t>(y) * pitch, static_cast<size_t>(width) * 4);
            }
            buffer2d->Unlock2D();
        } else {
            // A plain buffer of RGB32 is bottom-up.
            BYTE* data = nullptr;
            DWORD length = 0;
            if (FAILED(buffer->Lock(&data, nullptr, &length))) return false;
            const size_t row = static_cast<size_t>(width) * 4;
            for (UINT32 y = 0; y < height && (y + 1) * row <= length; ++y) {
                std::memcpy(top_down.data() + y * row, data + (height - 1 - y) * row, row);
            }
            buffer->Unlock();
        }
        return write_poster_jpeg(top_down.data(), width, height, width * 4, jpeg);
    }
    MELLO_LOG_ERROR(TAG, "poster: no frame at %.2f s in %s", static_cast<double>(time_us) / 1e6,
                    mp4.string().c_str());
    return false;
}

} // namespace mello::replay
