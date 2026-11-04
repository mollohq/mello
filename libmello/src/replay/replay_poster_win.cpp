// The game clip poster JPEG on Windows (CLIPS.md 15.5): WIC.
#include "replay_mux.hpp"
#include "../util/log.hpp"

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <wincodec.h>
#include <wrl/client.h>
#include <algorithm>
#include <cstring>
#include <system_error>

using Microsoft::WRL::ComPtr;

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/poster";

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

} // namespace mello::replay
