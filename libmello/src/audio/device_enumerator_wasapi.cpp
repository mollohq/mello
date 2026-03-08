#ifdef _WIN32
#include "device_enumerator.hpp"
#include "../util/log.hpp"
#include <mmdeviceapi.h>
#include <functiondiscoverykeys_devpkey.h>
#include <combaseapi.h>
#include <string>
#include <vector>

namespace mello::audio {

static std::string wide_to_utf8(const wchar_t* wide) {
    if (!wide) return {};
    int len = WideCharToMultiByte(CP_UTF8, 0, wide, -1, nullptr, 0, nullptr, nullptr);
    if (len <= 0) return {};
    std::string out(len - 1, '\0');
    WideCharToMultiByte(CP_UTF8, 0, wide, -1, out.data(), len, nullptr, nullptr);
    return out;
}

static std::wstring utf8_to_wide(const char* utf8) {
    if (!utf8) return {};
    int len = MultiByteToWideChar(CP_UTF8, 0, utf8, -1, nullptr, 0);
    if (len <= 0) return {};
    std::wstring out(len - 1, L'\0');
    MultiByteToWideChar(CP_UTF8, 0, utf8, -1, out.data(), len);
    return out;
}

class WasapiDeviceEnumerator : public AudioDeviceEnumerator {
public:
    std::vector<AudioDeviceInfo> list_capture_devices() override {
        return enumerate(eCapture);
    }

    std::vector<AudioDeviceInfo> list_playback_devices() override {
        return enumerate(eRender);
    }

private:
    std::vector<AudioDeviceInfo> enumerate(EDataFlow flow) {
        std::vector<AudioDeviceInfo> result;

        HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        bool did_init = SUCCEEDED(hr);

        IMMDeviceEnumerator* enumerator = nullptr;
        hr = CoCreateInstance(
            __uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
            __uuidof(IMMDeviceEnumerator), reinterpret_cast<void**>(&enumerator));
        if (FAILED(hr)) {
            if (did_init) CoUninitialize();
            return result;
        }

        // Get default device id for comparison
        std::string default_id;
        {
            IMMDevice* def_dev = nullptr;
            if (SUCCEEDED(enumerator->GetDefaultAudioEndpoint(flow, eCommunications, &def_dev))) {
                LPWSTR wid = nullptr;
                if (SUCCEEDED(def_dev->GetId(&wid))) {
                    default_id = wide_to_utf8(wid);
                    CoTaskMemFree(wid);
                }
                def_dev->Release();
            }
        }

        IMMDeviceCollection* collection = nullptr;
        hr = enumerator->EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE, &collection);
        if (FAILED(hr)) {
            enumerator->Release();
            if (did_init) CoUninitialize();
            return result;
        }

        UINT count = 0;
        collection->GetCount(&count);

        for (UINT i = 0; i < count; ++i) {
            IMMDevice* device = nullptr;
            if (FAILED(collection->Item(i, &device))) continue;

            AudioDeviceInfo info;

            LPWSTR wid = nullptr;
            if (SUCCEEDED(device->GetId(&wid))) {
                info.id = wide_to_utf8(wid);
                CoTaskMemFree(wid);
            }

            IPropertyStore* props = nullptr;
            if (SUCCEEDED(device->OpenPropertyStore(STGM_READ, &props))) {
                PROPVARIANT name_var;
                PropVariantInit(&name_var);
                if (SUCCEEDED(props->GetValue(PKEY_Device_FriendlyName, &name_var))) {
                    if (name_var.vt == VT_LPWSTR && name_var.pwszVal) {
                        info.name = wide_to_utf8(name_var.pwszVal);
                    }
                }
                PropVariantClear(&name_var);
                props->Release();
            }

            info.is_default = (info.id == default_id);
            result.push_back(std::move(info));
            device->Release();
        }

        collection->Release();
        enumerator->Release();
        if (did_init) CoUninitialize();

        MELLO_LOG_INFO("devices", "enumerated %zu %s devices",
                       result.size(), flow == eCapture ? "capture" : "playback");
        for (size_t i = 0; i < result.size(); ++i) {
            MELLO_LOG_DEBUG("devices", "  [%zu] %s%s: %s",
                            i, result[i].name.c_str(),
                            result[i].is_default ? " (default)" : "",
                            result[i].id.c_str());
        }
        return result;
    }
};

std::unique_ptr<AudioDeviceEnumerator> create_device_enumerator() {
    return std::make_unique<WasapiDeviceEnumerator>();
}

// Helper: open a specific device by utf8 ID, or default if null/empty
IMMDevice* open_device_by_id(EDataFlow flow, const char* device_id) {
    IMMDeviceEnumerator* enumerator = nullptr;
    HRESULT hr = CoCreateInstance(
        __uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
        __uuidof(IMMDeviceEnumerator), reinterpret_cast<void**>(&enumerator));
    if (FAILED(hr)) return nullptr;

    IMMDevice* device = nullptr;
    if (device_id && device_id[0] != '\0') {
        std::wstring wid = utf8_to_wide(device_id);
        hr = enumerator->GetDevice(wid.c_str(), &device);
    } else {
        hr = enumerator->GetDefaultAudioEndpoint(flow, eCommunications, &device);
    }
    enumerator->Release();
    return SUCCEEDED(hr) ? device : nullptr;
}

} // namespace mello::audio
#endif
