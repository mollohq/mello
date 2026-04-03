#ifdef _WIN32
#include "audio_session_win.hpp"
#include "../util/log.hpp"

namespace mello::audio {

static const IID IID_IAudioSessionManager2_ = __uuidof(IAudioSessionManager2);
static const IID IID_IAudioSessionControl2_ = __uuidof(IAudioSessionControl2);

AudioSessionWin::AudioSessionWin() = default;

AudioSessionWin::~AudioSessionWin() {
    shutdown();
}

bool AudioSessionWin::initialize() {
    HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    if (FAILED(hr) && hr != S_FALSE && hr != RPC_E_CHANGED_MODE) {
        MELLO_LOG_ERROR("audio_session", "COM init failed hr=0x%08lx", hr);
        return false;
    }

    ComPtr<IMMDeviceEnumerator> enumerator;
    hr = CoCreateInstance(
        __uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
        __uuidof(IMMDeviceEnumerator),
        reinterpret_cast<void**>(enumerator.GetAddressOf()));
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("audio_session", "CoCreateInstance(MMDeviceEnumerator) failed hr=0x%08lx", hr);
        return false;
    }

    ComPtr<IMMDevice> device;
    hr = enumerator->GetDefaultAudioEndpoint(eRender, eMultimedia, &device);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("audio_session", "GetDefaultAudioEndpoint failed hr=0x%08lx", hr);
        return false;
    }

    hr = device->Activate(
        IID_IAudioSessionManager2_, CLSCTX_ALL, nullptr,
        reinterpret_cast<void**>(session_manager_.GetAddressOf()));
    if (FAILED(hr)) {
        MELLO_LOG_ERROR("audio_session", "Activate(IAudioSessionManager2) failed hr=0x%08lx", hr);
        return false;
    }

    hr = session_manager_->RegisterDuckNotification(nullptr, this);
    if (FAILED(hr)) {
        MELLO_LOG_WARN("audio_session", "RegisterDuckNotification failed hr=0x%08lx (ducking preference still applies)", hr);
    } else {
        registered_ = true;
        MELLO_LOG_INFO("audio_session", "duck notification handler registered");
    }

    return true;
}

bool AudioSessionWin::disable_ducking_for_client(IAudioClient* client) {
    if (!client) return false;

    ComPtr<IAudioSessionControl> session_control;
    HRESULT hr = client->GetService(
        __uuidof(IAudioSessionControl),
        reinterpret_cast<void**>(session_control.GetAddressOf()));
    if (FAILED(hr)) {
        MELLO_LOG_WARN("audio_session", "GetService(IAudioSessionControl) failed hr=0x%08lx", hr);
        return false;
    }

    ComPtr<IAudioSessionControl2> session_control2;
    hr = session_control->QueryInterface(
        IID_IAudioSessionControl2_,
        reinterpret_cast<void**>(session_control2.GetAddressOf()));
    if (FAILED(hr)) {
        MELLO_LOG_WARN("audio_session", "QI(IAudioSessionControl2) failed hr=0x%08lx", hr);
        return false;
    }

    hr = session_control2->SetDuckingPreference(TRUE);
    if (FAILED(hr)) {
        MELLO_LOG_WARN("audio_session", "SetDuckingPreference(TRUE) failed hr=0x%08lx", hr);
        return false;
    }

    MELLO_LOG_INFO("audio_session", "ducking opt-out set for audio client");
    return true;
}

void AudioSessionWin::shutdown() {
    if (registered_ && session_manager_) {
        session_manager_->UnregisterDuckNotification(this);
        registered_ = false;
        MELLO_LOG_INFO("audio_session", "duck notification handler unregistered");
    }
    session_manager_.Reset();
}

// IUnknown

ULONG STDMETHODCALLTYPE AudioSessionWin::AddRef() {
    return InterlockedIncrement(&ref_count_);
}

ULONG STDMETHODCALLTYPE AudioSessionWin::Release() {
    ULONG count = InterlockedDecrement(&ref_count_);
    // prevent COM release from deleting — AudioPipeline owns our lifetime
    return count;
}

HRESULT STDMETHODCALLTYPE AudioSessionWin::QueryInterface(REFIID riid, void** ppv) {
    if (!ppv) return E_POINTER;

    if (riid == __uuidof(IUnknown) || riid == __uuidof(IAudioVolumeDuckNotification)) {
        *ppv = static_cast<IAudioVolumeDuckNotification*>(this);
        AddRef();
        return S_OK;
    }

    *ppv = nullptr;
    return E_NOINTERFACE;
}

// IAudioVolumeDuckNotification — intentional no-ops

HRESULT STDMETHODCALLTYPE AudioSessionWin::OnVolumeDuckNotification(
    LPCWSTR /*session_id*/,
    UINT32 /*countCommunicationSessions*/)
{
    MELLO_LOG_DEBUG("audio_session", "OnVolumeDuckNotification suppressed");
    return S_OK;
}

HRESULT STDMETHODCALLTYPE AudioSessionWin::OnVolumeUnduckNotification(
    LPCWSTR /*session_id*/)
{
    MELLO_LOG_DEBUG("audio_session", "OnVolumeUnduckNotification (no-op)");
    return S_OK;
}

} // namespace mello::audio
#endif
