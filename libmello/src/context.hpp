#pragma once
#include <string>

namespace mello {

class Context {
public:
    Context() = default;
    ~Context() = default;

    bool initialize() {
        // TODO: Initialize subsystems
        return true;
    }

    void shutdown() {
        // TODO: Shutdown subsystems
    }

    const char* get_error() const {
        return last_error_.c_str();
    }

    void set_error(const std::string& error) {
        last_error_ = error;
    }

private:
    std::string last_error_;
};

} // namespace mello
