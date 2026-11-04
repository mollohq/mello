// The game clip poster on Windows (CLIPS.md 15.5): WIC for the JPEG, and
// the Media Foundation source reader for a frame of the MP4.
#include "replay_mux.hpp"
#include "../util/log.hpp"

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
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

constexpr const char* TAG = "replay/poster";

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

} // namespace

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
