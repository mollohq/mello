#pragma once

#include "mello.h"

#include <string>

namespace mello::transport {

// Voice SFU tracks use msid = sender user UUID (contains '-').
// Stream SFU egress uses msid = session id ("stream_<user>_<ts>").
// Pion undeclared-SSRC phantoms lack both shapes and are dropped.
inline bool is_valid_incoming_audio_sender(
    MelloPeerMediaRole role,
    const std::string& sender_id
) {
    if (sender_id.empty() || sender_id == "unknown") {
        return false;
    }
    if (role == MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER) {
        return sender_id.rfind("stream_", 0) == 0;
    }
    if (role == MELLO_PEER_MEDIA_ROLE_VOICE) {
        return sender_id.find('-') != std::string::npos;
    }
    return false;
}

}  // namespace mello::transport
