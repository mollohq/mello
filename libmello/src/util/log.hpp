#pragma once
#include <cstdio>
#include <cstdarg>

namespace mello {

enum class LogLevel { Debug, Info, Warn, Error };

inline LogLevel g_log_level = LogLevel::Info;

inline void set_log_level(LogLevel level) { g_log_level = level; }

inline void log(LogLevel level, const char* tag, const char* fmt, ...) {
    if (level < g_log_level) return;
    const char* prefix = "";
    switch (level) {
        case LogLevel::Debug: prefix = "DEBUG"; break;
        case LogLevel::Info:  prefix = "INFO";  break;
        case LogLevel::Warn:  prefix = "WARN";  break;
        case LogLevel::Error: prefix = "ERROR"; break;
    }
    fprintf(stderr, "[libmello::%s] %s: ", tag, prefix);
    va_list args;
    va_start(args, fmt);
    vfprintf(stderr, fmt, args);
    va_end(args);
    fprintf(stderr, "\n");
    fflush(stderr);
}

} // namespace mello

#define MELLO_LOG_DEBUG(tag, ...) mello::log(mello::LogLevel::Debug, tag, __VA_ARGS__)
#define MELLO_LOG_INFO(tag, ...)  mello::log(mello::LogLevel::Info,  tag, __VA_ARGS__)
#define MELLO_LOG_WARN(tag, ...)  mello::log(mello::LogLevel::Warn,  tag, __VA_ARGS__)
#define MELLO_LOG_ERROR(tag, ...) mello::log(mello::LogLevel::Error, tag, __VA_ARGS__)
