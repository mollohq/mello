#pragma once
// iOS audio session helper (IOS-LIBMELLO-PORT §5). The RemoteIO capture/playback
// units share one process-wide AVAudioSession; both call configure_voice_session()
// from initialize() before AudioUnitInitialize. Idempotent: the session is a
// singleton, so repeated configure/activate calls are cheap and safe.
namespace mello::audio {

// Configure category .playAndRecord / mode .voiceChat and activate the session.
// Returns false if activation fails (e.g. permission not yet granted).
bool configure_voice_session();

// Deactivate the shared session (best-effort; called when the last unit stops).
void deactivate_voice_session();

} // namespace mello::audio
