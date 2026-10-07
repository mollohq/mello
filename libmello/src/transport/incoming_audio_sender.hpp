#pragma once

#include <string>

namespace mello::transport {

// Stream SFU egress names its audio track msid after the session id
// ("stream_<user>_<ts>"). A stream viewer drops other incoming audio tracks
// (Pion undeclared-SSRC phantoms). Voice does not use msid: it resolves the
// sender by mid from the SFU slot map (voice_slot_map.hpp).
inline bool is_stream_audio_sender(const std::string& sender_id) {
    return sender_id.rfind("stream_", 0) == 0;
}

}  // namespace mello::transport
