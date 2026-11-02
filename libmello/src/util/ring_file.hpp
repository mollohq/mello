#pragma once
#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <string>

namespace mello::util {

// A file of a fixed size with positioned reads and writes, for the clip rings
// on disk (the voice ring, the replay video and game audio rings). The file
// goes away when it is closed, and when the process dies: it is opened delete
// on close (Windows) or unlinked after open (POSIX), so a crash leaves no file.
//
// Threads: one writer and any number of readers may use it at once. Each call
// is one positioned read or write.
class RingFile {
public:
    RingFile() = default;
    ~RingFile() { close(); }
    RingFile(const RingFile&) = delete;
    RingFile& operator=(const RingFile&) = delete;

    // Create the file at `path` with `size` bytes. False when it cannot.
    bool open(const std::filesystem::path& path, uint64_t size);
    bool write_at(uint64_t offset, const void* data, size_t size);
    bool read_at(uint64_t offset, void* data, size_t size);
    void close();
    bool is_open() const;

private:
#ifdef _WIN32
    void* handle_ = reinterpret_cast<void*>(static_cast<intptr_t>(-1)); // INVALID_HANDLE_VALUE
#else
    int fd_ = -1;
#endif
};

// The clip rings' file name: `<prefix>_<pid>_<n>.bin`. The pid tells the
// files of two clients on one PC apart; `n` counts up in the process.
std::string ring_file_name(const char* prefix);

// The OS temp folder's `mello_clips` subfolder, the default folder of the
// clip rings (CLIPS.md 4.4). Empty when there is no temp folder.
std::filesystem::path default_clip_dir();

} // namespace mello::util
