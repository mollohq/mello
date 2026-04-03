#pragma once

#ifdef _WIN32
#include <audiopolicy.h>
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <wrl/client.h>

using Microsoft::WRL::ComPtr;

namespace mello::audio {

/// Prevents Windows from ducking other applications when mello
/// opens communications audio sessions.
///
/// Two layers of defense:
///   1. SetDuckingPreference(TRUE) on each audio session
///   2. IAudioVolumeDuckNotification to intercept and undo
///      system-level ducking that bypasses the preference
class AudioSessionWin : public IAudioVolumeDuckNotification {
public:
    AudioSessionWin();
    ~AudioSessionWin();

    /// Call once during AudioPipeline::initialize().
    /// Registers the duck notification handler with the default
    /// multimedia audio endpoint.
    bool initialize();

    /// Call on each IAudioClient after Initialize() but before Start().
    /// Sets the ducking opt-out preference on that client's session.
    bool disable_ducking_for_client(IAudioClient* client);

    /// Call during AudioPipeline::shutdown().
    void shutdown();

    // IUnknown
    ULONG STDMETHODCALLTYPE AddRef() override;
    ULONG STDMETHODCALLTYPE Release() override;
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void** ppv) override;

    // IAudioVolumeDuckNotification — both are intentional no-ops.
    // By not acting on the notification, ducking is suppressed for
    // sessions that have SetDuckingPreference(TRUE).
    HRESULT STDMETHODCALLTYPE OnVolumeDuckNotification(
        LPCWSTR session_id,
        UINT32 countCommunicationSessions
    ) override;

    HRESULT STDMETHODCALLTYPE OnVolumeUnduckNotification(
        LPCWSTR session_id
    ) override;

private:
    ComPtr<IAudioSessionManager2> session_manager_;
    LONG ref_count_ = 1;
    bool registered_ = false;
};

} // namespace mello::audio
#endif
