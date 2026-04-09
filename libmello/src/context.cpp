#include "context.hpp"
#include "util/log.hpp"

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

void Context::shutdown() {
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
