#include "ring_file.hpp"
#include <atomic>
#include <string>
#include <system_error>

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#else
#include <fcntl.h>
#include <unistd.h>
#endif

namespace mello::util {

bool RingFile::open(const std::filesystem::path& path, uint64_t size) {
    close();
#ifdef _WIN32
    HANDLE h = CreateFileW(path.c_str(), GENERIC_READ | GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS,
                           FILE_ATTRIBUTE_NORMAL | FILE_FLAG_DELETE_ON_CLOSE, nullptr);
    if (h == INVALID_HANDLE_VALUE) return false;
    handle_ = h;
    LARGE_INTEGER end;
    end.QuadPart = static_cast<LONGLONG>(size);
    if (!SetFilePointerEx(h, end, nullptr, FILE_BEGIN) || !SetEndOfFile(h)) {
        close();
        return false;
    }
    return true;
#else
    fd_ = ::open(path.c_str(), O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    if (fd_ < 0) return false;
    // The open descriptor keeps the data. No name is left to clean up.
    ::unlink(path.c_str());
    if (::ftruncate(fd_, static_cast<off_t>(size)) != 0) {
        close();
        return false;
    }
    return true;
#endif
}

bool RingFile::write_at(uint64_t offset, const void* data, size_t size) {
#ifdef _WIN32
    OVERLAPPED at{};
    at.Offset = static_cast<DWORD>(offset & 0xFFFFFFFFu);
    at.OffsetHigh = static_cast<DWORD>(offset >> 32);
    DWORD done = 0;
    return WriteFile(static_cast<HANDLE>(handle_), data, static_cast<DWORD>(size), &done, &at) &&
           done == size;
#else
    const auto* p = static_cast<const uint8_t*>(data);
    while (size > 0) {
        const ssize_t n = ::pwrite(fd_, p, size, static_cast<off_t>(offset));
        if (n <= 0) return false;
        p += n;
        offset += static_cast<uint64_t>(n);
        size -= static_cast<size_t>(n);
    }
    return true;
#endif
}

bool RingFile::read_at(uint64_t offset, void* data, size_t size) {
#ifdef _WIN32
    OVERLAPPED at{};
    at.Offset = static_cast<DWORD>(offset & 0xFFFFFFFFu);
    at.OffsetHigh = static_cast<DWORD>(offset >> 32);
    DWORD done = 0;
    return ReadFile(static_cast<HANDLE>(handle_), data, static_cast<DWORD>(size), &done, &at) &&
           done == size;
#else
    auto* p = static_cast<uint8_t*>(data);
    while (size > 0) {
        const ssize_t n = ::pread(fd_, p, size, static_cast<off_t>(offset));
        if (n <= 0) return false;
        p += n;
        offset += static_cast<uint64_t>(n);
        size -= static_cast<size_t>(n);
    }
    return true;
#endif
}

void RingFile::close() {
#ifdef _WIN32
    if (handle_ != INVALID_HANDLE_VALUE) {
        CloseHandle(static_cast<HANDLE>(handle_));
        handle_ = INVALID_HANDLE_VALUE;
    }
#else
    if (fd_ >= 0) {
        ::close(fd_);
        fd_ = -1;
    }
#endif
}

bool RingFile::is_open() const {
#ifdef _WIN32
    return handle_ != INVALID_HANDLE_VALUE;
#else
    return fd_ >= 0;
#endif
}

std::string ring_file_name(const char* prefix) {
    static std::atomic<uint32_t> seq{0};
#ifdef _WIN32
    const unsigned long pid = GetCurrentProcessId();
#else
    const unsigned long pid = static_cast<unsigned long>(getpid());
#endif
    return std::string(prefix) + "_" + std::to_string(pid) + "_" +
           std::to_string(seq.fetch_add(1, std::memory_order_relaxed)) + ".bin";
}

std::filesystem::path default_clip_dir() {
    std::error_code ec;
    auto tmp = std::filesystem::temp_directory_path(ec);
    if (ec) return {};
    return tmp / "mello_clips";
}

} // namespace mello::util
