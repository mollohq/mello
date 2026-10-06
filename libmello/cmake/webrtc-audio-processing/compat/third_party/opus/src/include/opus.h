// Compat shim: upstream WebRTC includes the Opus headers from its own
// third_party/opus checkout. libmello takes Opus from vcpkg, whose
// Opus::opus target puts include/opus on the include path.
#pragma once
#include <opus.h>
