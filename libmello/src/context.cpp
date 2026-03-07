#include "context.hpp"

namespace mello {

bool Context::initialize() {
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
