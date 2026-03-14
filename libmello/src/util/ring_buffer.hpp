#pragma once
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <vector>
#include <mutex>
#include <cstring>
#include <algorithm>

namespace mello::util {

template <typename T>
class RingBuffer {
public:
    explicit RingBuffer(size_t capacity)
        : buf_(capacity), capacity_(capacity) {}

    size_t write(const T* data, size_t count) {
        std::lock_guard<std::mutex> lock(mtx_);
        size_t to_write = std::min(count, capacity_ - size_);
        for (size_t i = 0; i < to_write; ++i) {
            buf_[(write_pos_ + i) % capacity_] = data[i];
        }
        write_pos_ = (write_pos_ + to_write) % capacity_;
        size_ += to_write;
        return to_write;
    }

    size_t read(T* data, size_t count) {
        std::lock_guard<std::mutex> lock(mtx_);
        size_t to_read = std::min(count, size_);
        for (size_t i = 0; i < to_read; ++i) {
            data[i] = buf_[(read_pos_ + i) % capacity_];
        }
        read_pos_ = (read_pos_ + to_read) % capacity_;
        size_ -= to_read;
        return to_read;
    }

    size_t available() const {
        std::lock_guard<std::mutex> lock(mtx_);
        return size_;
    }

    size_t free_space() const {
        std::lock_guard<std::mutex> lock(mtx_);
        return capacity_ - size_;
    }

    void clear() {
        std::lock_guard<std::mutex> lock(mtx_);
        read_pos_ = 0;
        write_pos_ = 0;
        size_ = 0;
    }

private:
    std::vector<T> buf_;
    size_t capacity_;
    size_t read_pos_ = 0;
    size_t write_pos_ = 0;
    size_t size_ = 0;
    mutable std::mutex mtx_;
};

} // namespace mello::util
