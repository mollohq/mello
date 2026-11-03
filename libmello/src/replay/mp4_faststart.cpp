#include "mp4_faststart.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <cstdint>
#include <cstring>
#include <fstream>
#include <string>
#include <system_error>
#include <vector>

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/faststart";
constexpr size_t kCopyBlock = 1 << 20;

struct Box {
    uint64_t offset = 0;
    uint64_t size = 0;
    char type[5] = {};
};

uint32_t be32(const uint8_t* p) {
    return (uint32_t{p[0]} << 24) | (uint32_t{p[1]} << 16) | (uint32_t{p[2]} << 8) | p[3];
}
uint64_t be64(const uint8_t* p) { return (uint64_t{be32(p)} << 32) | be32(p + 4); }
void put32(uint8_t* p, uint32_t v) {
    p[0] = static_cast<uint8_t>(v >> 24);
    p[1] = static_cast<uint8_t>(v >> 16);
    p[2] = static_cast<uint8_t>(v >> 8);
    p[3] = static_cast<uint8_t>(v);
}
void put64(uint8_t* p, uint64_t v) {
    put32(p, static_cast<uint32_t>(v >> 32));
    put32(p + 4, static_cast<uint32_t>(v));
}

// The top-level boxes of the file.
bool read_boxes(std::ifstream& in, uint64_t file_size, std::vector<Box>& boxes) {
    uint64_t at = 0;
    while (at + 8 <= file_size) {
        uint8_t h[16];
        in.seekg(static_cast<std::streamoff>(at));
        in.read(reinterpret_cast<char*>(h), 8);
        if (!in) return false;
        Box box;
        box.offset = at;
        box.size = be32(h);
        std::memcpy(box.type, h + 4, 4);
        if (box.size == 1) {
            in.read(reinterpret_cast<char*>(h + 8), 8);
            if (!in) return false;
            box.size = be64(h + 8);
        } else if (box.size == 0) {
            box.size = file_size - at;  // to the end of the file
        }
        if (box.size < 8 || at + box.size > file_size) return false;
        boxes.push_back(box);
        at += box.size;
    }
    return at == file_size;
}

// Add `delta` to every chunk offset under the box at `data[begin, end)`.
// The containers on the way down to stbl: moov, trak, mdia, minf, stbl.
bool shift_offsets(std::vector<uint8_t>& data, size_t begin, size_t end, uint64_t delta) {
    size_t at = begin;
    while (at + 8 <= end) {
        uint64_t size = be32(&data[at]);
        size_t header = 8;
        if (size == 1) {
            if (at + 16 > end) return false;
            size = be64(&data[at + 8]);
            header = 16;
        }
        if (size < header || at + size > end) return false;
        const std::string type(reinterpret_cast<const char*>(&data[at + 4]), 4);
        const size_t body = at + header;
        const size_t box_end = static_cast<size_t>(at + size);
        if (type == "trak" || type == "mdia" || type == "minf" || type == "stbl") {
            if (!shift_offsets(data, body, box_end, delta)) return false;
        } else if (type == "stco" || type == "co64") {
            // version and flags, then the entry count, then the entries.
            if (body + 8 > box_end) return false;
            const uint32_t count = be32(&data[body + 4]);
            const size_t entry = type == "stco" ? 4 : 8;
            if (body + 8 + static_cast<size_t>(count) * entry > box_end) return false;
            for (uint32_t i = 0; i < count; ++i) {
                uint8_t* p = &data[body + 8 + static_cast<size_t>(i) * entry];
                if (entry == 4) {
                    const uint64_t moved = uint64_t{be32(p)} + delta;
                    if (moved > UINT32_MAX) return false;
                    put32(p, static_cast<uint32_t>(moved));
                } else {
                    put64(p, be64(p) + delta);
                }
            }
        }
        at = box_end;
    }
    return true;
}

bool copy_range(std::ifstream& in, std::ofstream& out, uint64_t offset, uint64_t size) {
    std::vector<char> block(kCopyBlock);
    in.seekg(static_cast<std::streamoff>(offset));
    while (size > 0) {
        const size_t n = static_cast<size_t>(std::min<uint64_t>(size, kCopyBlock));
        in.read(block.data(), static_cast<std::streamsize>(n));
        if (!in) return false;
        out.write(block.data(), static_cast<std::streamsize>(n));
        if (!out) return false;
        size -= n;
    }
    return true;
}

} // namespace

bool mp4_faststart(const std::filesystem::path& src, const std::filesystem::path& dst) {
    std::error_code ec;
    const uint64_t file_size = std::filesystem::file_size(src, ec);
    if (ec) return false;
    std::ifstream in(src, std::ios::binary);
    if (!in) return false;

    std::vector<Box> boxes;
    if (!read_boxes(in, file_size, boxes)) {
        MELLO_LOG_ERROR(TAG, "%s: not a plain MP4", src.string().c_str());
        return false;
    }
    int moov = -1, mdat = -1;
    for (size_t i = 0; i < boxes.size(); ++i) {
        if (std::strcmp(boxes[i].type, "moov") == 0) {
            if (moov >= 0) return false;
            moov = static_cast<int>(i);
        }
        if (std::strcmp(boxes[i].type, "mdat") == 0) {
            if (mdat >= 0) return false;
            mdat = static_cast<int>(i);
        }
    }
    if (moov < 0 || mdat < 0) {
        MELLO_LOG_ERROR(TAG, "%s: no moov or no mdat", src.string().c_str());
        return false;
    }

    std::vector<uint8_t> moov_data(static_cast<size_t>(boxes[moov].size));
    in.seekg(static_cast<std::streamoff>(boxes[moov].offset));
    in.read(reinterpret_cast<char*>(moov_data.data()), static_cast<std::streamsize>(moov_data.size()));
    if (!in) return false;

    // moov already before mdat: a plain copy.
    if (moov < mdat) {
        std::filesystem::copy_file(src, dst, std::filesystem::copy_options::overwrite_existing, ec);
        return !ec;
    }

    // Every box from mdat up to moov moves later by the size of moov.
    const size_t header = be32(moov_data.data()) == 1 ? 16 : 8;
    if (!shift_offsets(moov_data, header, moov_data.size(), boxes[moov].size)) {
        MELLO_LOG_ERROR(TAG, "%s: a chunk offset does not fit", src.string().c_str());
        return false;
    }

    std::ofstream out(dst, std::ios::binary | std::ios::trunc);
    if (!out) return false;
    for (size_t i = 0; i < boxes.size(); ++i) {
        if (static_cast<int>(i) == moov) continue;
        if (static_cast<int>(i) == mdat) {
            out.write(reinterpret_cast<const char*>(moov_data.data()),
                      static_cast<std::streamsize>(moov_data.size()));
        }
        if (!copy_range(in, out, boxes[i].offset, boxes[i].size)) {
            MELLO_LOG_ERROR(TAG, "%s: copy failed", src.string().c_str());
            return false;
        }
    }
    out.flush();
    return static_cast<bool>(out);
}

} // namespace mello::replay
