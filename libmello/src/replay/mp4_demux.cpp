#include "mp4_demux.hpp"
#include "mp4_box.hpp"

#include <algorithm>
#include <fstream>
#include <memory>
#include <mutex>
#include <system_error>

namespace mello::replay {

namespace {

/// A game clip's moov is about 100 KB for 30 s. A moov over 32 MiB is not a
/// clip of ours: refuse it rather than allocate it.
constexpr uint64_t kMaxMoov = 32ull << 20;
/// Samples per track: 31 s at 240 fps is under 8000. A table over this
/// limit is refused.
constexpr uint32_t kMaxSamples = 2'000'000;

/// A bounds-checked reader over one box body. A read past the end sets
/// `bad` and returns 0, so a parse checks `ok()` once at the end of a box.
class Cursor {
public:
    Cursor(const uint8_t* data, size_t size) : data_(data), size_(size) {}
    bool ok() const { return !bad_; }
    size_t left() const { return bad_ ? 0 : size_ - at_; }
    const uint8_t* here() const { return data_ + at_; }
    void skip(size_t n) {
        if (n > left()) {
            bad_ = true;
            return;
        }
        at_ += n;
    }
    uint8_t u8() { return take(1) ? data_[at_ - 1] : 0; }
    uint16_t u16() { return take(2) ? mp4::be16(data_ + at_ - 2) : 0; }
    uint32_t u24() { return take(3) ? (uint32_t{data_[at_ - 3]} << 16) | mp4::be16(data_ + at_ - 2) : 0; }
    uint32_t u32() { return take(4) ? mp4::be32(data_ + at_ - 4) : 0; }
    uint64_t u64() { return take(8) ? mp4::be64(data_ + at_ - 8) : 0; }

private:
    bool take(size_t n) {
        if (n > left()) {
            bad_ = true;
            return false;
        }
        at_ += n;
        return true;
    }
    const uint8_t* data_;
    size_t size_;
    size_t at_ = 0;
    bool bad_ = false;
};

void fail(std::string* error, const std::string& text) {
    if (error && error->empty()) *error = text;
}

/// The sample tables of one track, as stored.
struct Tables {
    std::vector<std::pair<uint32_t, uint32_t>> stts;  // count, delta
    std::vector<std::pair<uint32_t, int32_t>> ctts;   // count, offset
    std::vector<uint32_t> stss;                       // 1-based
    bool has_stss = false;
    struct StscEntry {
        uint32_t first_chunk = 0;
        uint32_t samples = 0;
    };
    std::vector<StscEntry> stsc;
    uint32_t stsz_fixed = 0;
    uint32_t stsz_count = 0;
    std::vector<uint32_t> stsz;
    std::vector<uint64_t> chunks;  // stco or co64
};

bool parse_avcc(const uint8_t* body, size_t size, Mp4Track& t) {
    Cursor c(body, size);
    if (c.u8() != 1) return false;  // configuration version
    c.skip(3);                      // profile, constraints, level
    t.nal_length_size = static_cast<uint8_t>((c.u8() & 3) + 1);
    const uint32_t sps_count = c.u8() & 0x1f;
    for (uint32_t i = 0; i < sps_count && c.ok(); ++i) {
        const uint16_t n = c.u16();
        if (n == 0 || n > c.left()) return false;
        t.sps.emplace_back(c.here(), c.here() + n);
        c.skip(n);
    }
    const uint32_t pps_count = c.u8();
    for (uint32_t i = 0; i < pps_count && c.ok(); ++i) {
        const uint16_t n = c.u16();
        if (n == 0 || n > c.left()) return false;
        t.pps.emplace_back(c.here(), c.here() + n);
        c.skip(n);
    }
    // The high profile fields after the PPS are not needed to decode.
    return c.ok() && t.nal_length_size != 3;
}

bool parse_dops(const uint8_t* body, size_t size, Mp4Track& t) {
    Cursor c(body, size);
    if (c.u8() != 0) return false;  // version
    t.channels = c.u8();
    t.pre_skip = c.u16();
    c.u32();  // the input rate: informational, Opus decodes at 48 kHz
    c.u16();  // output gain
    const uint8_t family = c.u8();
    // Family 0 is mono or stereo with no mapping table. m3llo writes nothing else.
    return c.ok() && family == 0 && (t.channels == 1 || t.channels == 2);
}

bool parse_stsd(const uint8_t* body, size_t size, Mp4Track& t) {
    Cursor c(body, size);
    c.u32();  // version, flags
    const uint32_t count = c.u32();
    if (!c.ok() || count == 0) return false;
    // The first entry is the one every sample uses (data reference 1).
    bool ok = true;
    bool first = true;
    mp4::for_each_box(c.here(), c.left(), [&](const mp4::BoxRef& entry) {
        if (!first) return;
        first = false;
        t.codec = entry.type;
        if (entry.type == mp4::fourcc("avc1") || entry.type == mp4::fourcc("avc3")) {
            constexpr size_t kVisual = 78;  // the VisualSampleEntry fields
            if (entry.body_size < kVisual) {
                ok = false;
                return;
            }
            t.width = mp4::be16(entry.body + 24);
            t.height = mp4::be16(entry.body + 26);
            bool have_avcc = false;
            ok = mp4::for_each_box(entry.body + kVisual, entry.body_size - kVisual, [&](const mp4::BoxRef& b) {
                if (b.type == mp4::fourcc("avcC") && !have_avcc) have_avcc = parse_avcc(b.body, b.body_size, t);
            });
            // avc3 may carry its parameter sets in band only.
            t.supported = ok && (have_avcc || entry.type == mp4::fourcc("avc3"));
        } else if (entry.type == mp4::fourcc("Opus")) {
            constexpr size_t kAudio = 28;  // the AudioSampleEntry fields
            if (entry.body_size < kAudio) {
                ok = false;
                return;
            }
            t.sample_rate = mp4::be32(entry.body + 24) >> 16;
            bool have_dops = false;
            ok = mp4::for_each_box(entry.body + kAudio, entry.body_size - kAudio, [&](const mp4::BoxRef& b) {
                if (b.type == mp4::fourcc("dOps") && !have_dops) have_dops = parse_dops(b.body, b.body_size, t);
            });
            t.supported = ok && have_dops;
        }
    });
    return ok;
}

bool parse_stbl(const uint8_t* body, size_t size, Mp4Track& t, Tables& tab) {
    bool ok = true;
    const bool walked = mp4::for_each_box(body, size, [&](const mp4::BoxRef& b) {
        if (!ok) return;
        Cursor c(b.body, b.body_size);
        if (b.type == mp4::fourcc("stsd")) {
            ok = parse_stsd(b.body, b.body_size, t);
            return;
        }
        if (b.type == mp4::fourcc("stts")) {
            c.u32();
            const uint32_t n = c.u32();
            if (n > c.left() / 8) {
                ok = false;
                return;
            }
            for (uint32_t i = 0; i < n; ++i) {
                const uint32_t count = c.u32();
                tab.stts.emplace_back(count, c.u32());
            }
        } else if (b.type == mp4::fourcc("ctts")) {
            c.u32();  // version, flags
            const uint32_t n = c.u32();
            if (n > c.left() / 8) {
                ok = false;
                return;
            }
            for (uint32_t i = 0; i < n; ++i) {
                const uint32_t count = c.u32();
                const uint32_t raw = c.u32();
                // Version 0 is unsigned, but writers put negative offsets in
                // it too: both versions read as signed.
                tab.ctts.emplace_back(count, static_cast<int32_t>(raw));
            }
        } else if (b.type == mp4::fourcc("stss")) {
            c.u32();
            const uint32_t n = c.u32();
            if (n > c.left() / 4) {
                ok = false;
                return;
            }
            tab.has_stss = true;
            for (uint32_t i = 0; i < n; ++i) tab.stss.push_back(c.u32());
        } else if (b.type == mp4::fourcc("stsc")) {
            c.u32();
            const uint32_t n = c.u32();
            if (n > c.left() / 12) {
                ok = false;
                return;
            }
            for (uint32_t i = 0; i < n; ++i) {
                Tables::StscEntry e;
                e.first_chunk = c.u32();
                e.samples = c.u32();
                c.u32();  // the sample description
                tab.stsc.push_back(e);
            }
        } else if (b.type == mp4::fourcc("stsz")) {
            c.u32();
            tab.stsz_fixed = c.u32();
            tab.stsz_count = c.u32();
            if (tab.stsz_count > kMaxSamples) {
                ok = false;
                return;
            }
            if (tab.stsz_fixed == 0) {
                if (tab.stsz_count > c.left() / 4) {
                    ok = false;
                    return;
                }
                tab.stsz.reserve(tab.stsz_count);
                for (uint32_t i = 0; i < tab.stsz_count; ++i) tab.stsz.push_back(c.u32());
            }
        } else if (b.type == mp4::fourcc("stco") || b.type == mp4::fourcc("co64")) {
            const bool wide = b.type == mp4::fourcc("co64");
            c.u32();
            const uint32_t n = c.u32();
            if (n > c.left() / (wide ? 8 : 4)) {
                ok = false;
                return;
            }
            for (uint32_t i = 0; i < n; ++i) tab.chunks.push_back(wide ? c.u64() : c.u32());
        } else if (b.type == mp4::fourcc("sgpd")) {
            const uint8_t version = c.u8();
            c.u24();
            if (c.u32() != mp4::fourcc("roll")) return;
            const uint32_t length = version == 1 ? c.u32() : 2;
            if (version >= 2) c.u32();  // the default description index
            if (c.u32() == 0) return;   // no entry
            if (version == 1 && length == 0) c.u32();
            t.roll_distance = static_cast<int16_t>(c.u16());
        }
        if (!c.ok()) ok = false;
    });
    return walked && ok;
}

/// The samples of a track from its tables, checked against the file size.
bool build_samples(const Tables& tab, uint64_t file_size, Mp4Track& t, std::string* error) {
    const uint32_t count = tab.stsz_count;
    if (count == 0) return true;
    if (tab.stsc.empty() || tab.chunks.empty() || tab.stts.empty()) {
        fail(error, "a track has samples but no chunk or time table");
        return false;
    }
    t.samples.resize(count);
    // Sizes and offsets: walk the chunks with stsc.
    uint32_t s = 0;
    for (size_t e = 0; e < tab.stsc.size() && s < count; ++e) {
        const uint32_t first = tab.stsc[e].first_chunk;
        const uint32_t last = e + 1 < tab.stsc.size() ? tab.stsc[e + 1].first_chunk
                                                      : static_cast<uint32_t>(tab.chunks.size() + 1);
        if (first == 0 || last < first || last > tab.chunks.size() + 1 || tab.stsc[e].samples == 0) {
            fail(error, "a bad stsc entry");
            return false;
        }
        for (uint32_t chunk = first; chunk < last && s < count; ++chunk) {
            uint64_t at = tab.chunks[chunk - 1];
            for (uint32_t k = 0; k < tab.stsc[e].samples && s < count; ++k, ++s) {
                const uint32_t size = tab.stsz_fixed ? tab.stsz_fixed : tab.stsz[s];
                if (at > file_size || size > file_size - at) {
                    fail(error, "a sample lies outside the file");
                    return false;
                }
                t.samples[s].offset = at;
                t.samples[s].size = size;
                at += size;
            }
        }
    }
    if (s != count) {
        fail(error, "the chunks hold fewer samples than stsz");
        return false;
    }
    // Times: stts gives decode times, ctts moves them to presentation times.
    int64_t dts = 0;
    s = 0;
    for (const auto& run : tab.stts) {
        for (uint32_t k = 0; k < run.first && s < count; ++k, ++s) {
            t.samples[s].time = dts;
            t.samples[s].duration = run.second;
            dts += run.second;
        }
    }
    for (; s < count; ++s) t.samples[s].time = dts;  // a short stts: the rest at its end
    s = 0;
    for (const auto& run : tab.ctts) {
        for (uint32_t k = 0; k < run.first && s < count; ++k, ++s) t.samples[s].time += run.second;
    }
    // Sync samples: no stss means every sample is one.
    if (!tab.has_stss) {
        for (auto& sample : t.samples) sample.sync = true;
    } else {
        for (uint32_t index : tab.stss) {
            if (index >= 1 && index <= count) t.samples[index - 1].sync = true;
        }
    }
    return true;
}

bool parse_trak(const uint8_t* body, size_t size, uint64_t file_size, Mp4Track& t, std::string* error) {
    Tables tab;
    bool ok = true;
    bool has_edit = false;
    bool walked = mp4::for_each_box(body, size, [&](const mp4::BoxRef& b) {
        if (!ok) return;
        if (b.type == mp4::fourcc("tkhd")) {
            Cursor c(b.body, b.body_size);
            const uint8_t version = c.u8();
            c.u24();
            c.skip(version == 1 ? 16 : 8);
            t.track_id = c.u32();
            ok = c.ok();
        } else if (b.type == mp4::fourcc("edts")) {
            ok = mp4::for_each_box(b.body, b.body_size, [&](const mp4::BoxRef& e) {
                if (e.type != mp4::fourcc("elst")) return;
                Cursor c(e.body, e.body_size);
                const uint8_t version = c.u8();
                c.u24();
                const uint32_t n = c.u32();
                for (uint32_t i = 0; i < n && c.ok(); ++i) {
                    const uint64_t duration = version == 1 ? c.u64() : c.u32();
                    const int64_t media = version == 1 ? static_cast<int64_t>(c.u64())
                                                       : static_cast<int32_t>(c.u32());
                    c.u32();  // rate
                    (void)duration;
                    // The first entry that is not an empty edit gives the start.
                    if (media >= 0) {
                        t.edit_media_time = media;
                        has_edit = true;
                        break;
                    }
                }
            });
        } else if (b.type == mp4::fourcc("mdia")) {
            ok = mp4::for_each_box(b.body, b.body_size, [&](const mp4::BoxRef& m) {
                if (m.type == mp4::fourcc("mdhd")) {
                    Cursor c(m.body, m.body_size);
                    const uint8_t version = c.u8();
                    c.u24();
                    c.skip(version == 1 ? 16 : 8);
                    t.timescale = c.u32();
                } else if (m.type == mp4::fourcc("hdlr")) {
                    Cursor c(m.body, m.body_size);
                    c.u32();
                    c.u32();
                    const uint32_t handler = c.u32();
                    if (handler == mp4::fourcc("vide")) t.kind = Mp4Track::Kind::Video;
                    if (handler == mp4::fourcc("soun")) t.kind = Mp4Track::Kind::Audio;
                } else if (m.type == mp4::fourcc("minf")) {
                    mp4::for_each_box(m.body, m.body_size, [&](const mp4::BoxRef& s) {
                        if (s.type == mp4::fourcc("stbl") && !parse_stbl(s.body, s.body_size, t, tab)) ok = false;
                    });
                }
            });
        }
    });
    if (!walked || !ok) {
        fail(error, "a track's boxes do not parse");
        return false;
    }
    if (t.timescale == 0) {
        fail(error, "a track has no timescale");
        return false;
    }
    // The edit's media time is in the track's timescale. With no edit list
    // an Opus track starts at its pre-skip. An edit list wins, as in a
    // browser: a media time of 0 plays the pre-skip.
    if (t.codec == mp4::fourcc("Opus") && !has_edit) t.edit_media_time = t.pre_skip;
    return build_samples(tab, file_size, t, error);
}

}  // namespace

const Mp4Track* Mp4Movie::video() const {
    for (const auto& t : tracks) {
        if (t.kind == Mp4Track::Kind::Video && t.supported) return &t;
    }
    return nullptr;
}

const Mp4Track* Mp4Movie::audio() const {
    for (const auto& t : tracks) {
        if (t.kind == Mp4Track::Kind::Audio && t.supported) return &t;
    }
    return nullptr;
}

bool parse_moov(const uint8_t* body, size_t size, uint64_t file_size, Mp4Movie& movie, std::string* error) {
    movie.tracks.clear();
    movie.file_size = file_size;
    uint32_t movie_scale = 0;
    uint64_t movie_duration = 0;
    bool ok = true;
    const bool walked = mp4::for_each_box(body, size, [&](const mp4::BoxRef& b) {
        if (!ok) return;
        if (b.type == mp4::fourcc("mvhd")) {
            Cursor c(b.body, b.body_size);
            const uint8_t version = c.u8();
            c.u24();
            c.skip(version == 1 ? 16 : 8);
            movie_scale = c.u32();
            movie_duration = version == 1 ? c.u64() : c.u32();
        } else if (b.type == mp4::fourcc("trak")) {
            if (movie.tracks.size() >= 16) return;  // a clip has two
            Mp4Track t;
            if (!parse_trak(b.body, b.body_size, file_size, t, error)) {
                ok = false;
                return;
            }
            movie.tracks.push_back(std::move(t));
        }
    });
    if (!walked || !ok) {
        fail(error, "the moov box does not parse");
        return false;
    }
    movie.duration_us = movie_scale ? movie_duration * 1'000'000 / movie_scale : 0;
    return true;
}

bool open_mp4(const Mp4ReadAt& read, uint64_t file_size, Mp4Movie& movie, std::string* error) {
    uint64_t at = 0;
    while (at + 8 <= file_size) {
        uint8_t h[16];
        if (!read(at, h, 8)) {
            fail(error, "a read failed");
            return false;
        }
        uint64_t box = mp4::be32(h);
        uint64_t header = 8;
        if (box == 1) {
            if (at + 16 > file_size || !read(at + 8, h + 8, 8)) {
                fail(error, "a read failed");
                return false;
            }
            box = mp4::be64(h + 8);
            header = 16;
        } else if (box == 0) {
            box = file_size - at;
        }
        if (box < header || box > file_size - at) {
            fail(error, "a top-level box does not fit the file");
            return false;
        }
        if (mp4::be32(h + 4) == mp4::fourcc("moov")) {
            const uint64_t body = box - header;
            if (body > kMaxMoov) {
                fail(error, "the moov box is too large");
                return false;
            }
            std::vector<uint8_t> moov(static_cast<size_t>(body));
            if (!read(at + header, moov.data(), moov.size())) {
                fail(error, "a read failed");
                return false;
            }
            return parse_moov(moov.data(), moov.size(), file_size, movie, error);
        }
        at += box;
    }
    fail(error, "no moov box");
    return false;
}

Mp4ReadAt file_reader(const std::filesystem::path& path, uint64_t* file_size) {
    std::error_code ec;
    const uint64_t size = std::filesystem::file_size(path, ec);
    if (file_size) *file_size = ec ? 0 : size;
    auto file = std::make_shared<std::ifstream>(path, std::ios::binary);
    auto mutex = std::make_shared<std::mutex>();
    return [file, mutex, size](uint64_t offset, uint8_t* buf, size_t len) {
        if (offset > size || len > size - offset) return false;
        std::lock_guard<std::mutex> lock(*mutex);
        if (!*file) file->clear();
        file->seekg(static_cast<std::streamoff>(offset));
        file->read(reinterpret_cast<char*>(buf), static_cast<std::streamsize>(len));
        return static_cast<size_t>(file->gcount()) == len;
    };
}

bool read_sample(const Mp4ReadAt& read, const Mp4Sample& sample, std::vector<uint8_t>& out) {
    out.resize(sample.size);
    return sample.size == 0 || read(sample.offset, out.data(), out.size());
}

bool avcc_to_annex_b(const uint8_t* data, size_t size, uint8_t length_size, std::vector<uint8_t>& out) {
    out.clear();
    if (length_size != 1 && length_size != 2 && length_size != 4) return false;
    out.reserve(size + 16);
    size_t at = 0;
    while (at < size) {
        if (size - at < length_size) return false;
        size_t n = 0;
        for (uint8_t i = 0; i < length_size; ++i) n = (n << 8) | data[at + i];
        at += length_size;
        if (n == 0 || n > size - at) return false;
        static const uint8_t kStart[] = {0, 0, 0, 1};
        out.insert(out.end(), kStart, kStart + 4);
        out.insert(out.end(), data + at, data + at + n);
        at += n;
    }
    return true;
}

}  // namespace mello::replay
