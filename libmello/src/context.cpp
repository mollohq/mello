#include "context.hpp"
#include "audio/shared_game_audio.hpp"
#include "util/log.hpp"

#if defined(_WIN32) || (defined(__APPLE__) && !defined(MELLO_IOS))
#define MELLO_HAS_REPLAY 1
#include "replay/replay_session.hpp"
#endif

#ifdef _WIN32
#include <Windows.h>

static bool s_seh_init_ok = false;

static int seh_filter(unsigned int code) {
    MELLO_LOG_ERROR("context", "audio init crashed (SEH exception 0x%08x)", code);
    return EXCEPTION_EXECUTE_HANDLER;
}

static void seh_wrapper(mello::Context* ctx) {
    __try {
        s_seh_init_ok = ctx->initialize_inner();
    } __except(seh_filter(GetExceptionCode())) {
        s_seh_init_ok = false;
    }
}
#endif

namespace mello {

bool Context::initialize() {
#ifdef _WIN32
    seh_wrapper(this);
    if (!s_seh_init_ok) {
        set_error("Audio initialization crashed or failed");
    }
    return s_seh_init_ok;
#else
    return initialize_inner();
#endif
}

bool Context::initialize_inner() {
    if (!audio_.initialize()) {
        set_error("Failed to initialize audio pipeline");
        return false;
    }
    return true;
}

std::shared_ptr<audio::SharedGameAudio> Context::game_audio(uint32_t pid) {
    if (pid == 0) return nullptr;
    std::lock_guard<std::mutex> lock(game_audio_mutex_);
    auto it = game_audio_.find(pid);
    if (it != game_audio_.end()) {
        if (auto live = it->second.lock()) return live;
    }
    auto opened = audio::SharedGameAudio::open(pid);
    // Old entries of games that ended go when they are found dead.
    for (auto e = game_audio_.begin(); e != game_audio_.end();) {
        e = e->second.expired() ? game_audio_.erase(e) : std::next(e);
    }
    if (opened) game_audio_[pid] = opened;
    return opened;
}

void Context::shutdown() {
#ifdef MELLO_HAS_REPLAY
    // Before the audio: the replay reads the clip buffer of the voice.
    if (auto replay = exchange_replay(nullptr)) replay->stop();
#endif
    audio_.shutdown();
}

void Context::set_error(const std::string& error) {
    std::lock_guard<std::mutex> lock(error_mutex_);
    last_error_ = error;
}

const char* Context::get_error() const {
    std::lock_guard<std::mutex> lock(error_mutex_);
    return last_error_.c_str();
}

} // namespace mello
