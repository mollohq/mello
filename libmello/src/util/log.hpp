#pragma once
#include <cstdio>
#include <cstdarg>
#include <atomic>
#include "mello.h"

namespace mello {

enum class LogLevel { Debug = 0, Info = 1, Warn = 2, Error = 3 };

inline LogLevel g_log_level = LogLevel::Info;

inline std::atomic<MelloLogCallback> g_log_callback{nullptr};
inline std::atomic<void*> g_log_user_data{nullptr};

inline void set_log_level(LogLevel level) { g_log_level = level; }

inline void set_log_callback(MelloLogCallback cb, void* ud) {
    g_log_user_data.store(ud, std::memory_order_release);
    g_log_callback.store(cb, std::memory_order_release);
}

inline void log(LogLevel level, const char* tag, const char* fmt, ...) {
    if (level < g_log_level) return;

    char buf[2048];
    va_list args;
    va_start(args, fmt);
    vsnprintf(buf, sizeof(buf), fmt, args);
    va_end(args);

    auto cb = g_log_callback.load(std::memory_order_acquire);
    if (cb) {
        auto ud = g_log_user_data.load(std::memory_order_acquire);
        cb(ud, static_cast<int>(level), tag, buf);
        return;
    }

    const char* prefix = "";
    switch (level) {
        case LogLevel::Debug: prefix = "DEBUG"; break;
        case LogLevel::Info:  prefix = "INFO";  break;
        case LogLevel::Warn:  prefix = "WARN";  break;
        case LogLevel::Error: prefix = "ERROR"; break;
    }
    fprintf(stderr, "[libmello::%s] %s: %s\n", tag, prefix, buf);
    fflush(stderr);
}

} // namespace mello

#define MELLO_LOG_DEBUG(tag, ...) mello::log(mello::LogLevel::Debug, tag, __VA_ARGS__)
#define MELLO_LOG_INFO(tag, ...)  mello::log(mello::LogLevel::Info,  tag, __VA_ARGS__)
#define MELLO_LOG_WARN(tag, ...)  mello::log(mello::LogLevel::Warn,  tag, __VA_ARGS__)
#define MELLO_LOG_ERROR(tag, ...) mello::log(mello::LogLevel::Error, tag, __VA_ARGS__)
