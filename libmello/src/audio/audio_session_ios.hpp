#pragma once
// iOS audio session helper (IOS-LIBMELLO-PORT §5). The RemoteIO capture/playback
// units share one process-wide AVAudioSession; both call configure_voice_session()
// from initialize() before AudioUnitInitialize. Idempotent: the session is a
// singleton, so repeated configure/activate calls are cheap and safe.
#include <functional>

namespace mello::audio {

// Configure category .playAndRecord / mode .voiceChat and activate the session.
// Returns false if activation fails (e.g. permission not yet granted).
bool configure_voice_session();

// Deactivate the shared session (best-effort; called when the last unit stops).
void deactivate_voice_session();

// Register a hook invoked after an audio interruption ends — the OS stops our
// IO units during an interruption (e.g. an incoming phone call), so we must
// reactivate the session and restart them. `token` keys the registration so a
// unit can remove its hook on stop().
void register_audio_restart(void* token, std::function<void()> restart);
void unregister_audio_restart(void* token);

} // namespace mello::audio
