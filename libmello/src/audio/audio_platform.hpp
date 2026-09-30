#pragma once

// Platform switches shared by the audio backends.

#ifdef __APPLE__
#include <TargetConditionals.h>
#endif

// The combined VoiceProcessingIO duplex backend (vpio_duplex.*) is macOS-only:
// it selects devices through the CoreAudio HAL (AudioDeviceID,
// kAudioOutputUnitProperty_CurrentDevice), which iOS does not have. iOS uses
// RemoteIO (capture_remoteio.*, playback_remoteio.*). CMake builds
// vpio_duplex.cpp for macOS only; this macro keeps shared code in line with it.
#if defined(__APPLE__) && !TARGET_OS_IPHONE
#define MELLO_HAS_VPIO_DUPLEX 1
#endif
