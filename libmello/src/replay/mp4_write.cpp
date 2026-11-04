// The game clip MP4 (CLIPS.md 15.4 and 15.5): m3llo's own writer, the same on
// every platform. H.264 passes through from the replay ring; the clip mix is
// encoded to Opus with libopus (Opus in ISOBMFF: the `Opus` sample entry and
// its `dOps` box). The writer knows every sample size before it writes, so
// it writes the moov box before the mdat box at once: a player can start
// before the download ends.

#include "replay_mux.hpp"
#include "h264_nal.hpp"
#include "mp4_box.hpp"
#include "../util/log.hpp"

#include <opus.h>

#include <algorithm>
#include <fstream>
#include <memory>
#include <string>
#include <system_error>
#include <vector>

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/mux";
constexpr uint32_t kMovieScale = 1000;  // mvhd, tkhd and elst: ms
constexpr uint32_t kVideoScale = 90000;
constexpr uint32_t kAudioRate = 48000;
constexpr uint32_t kOpusFrame = 960;  // 20 ms
/// 80 ms of pre-roll, in Opus frames: a decoder that starts at a sample
/// needs the 4 frames before it (the Opus encapsulation spec, `roll`).
constexpr int16_t kOpusRoll = -4;
constexpr uint64_t kChunkUs = 500'000;

struct OpusEncoderDeleter {
    void operator()(OpusEncoder* e) const { opus_encoder_destroy(e); }
};

/// The NAL units of an Annex-B access unit with 4-byte lengths (AVCC).
size_t avcc_size(const std::vector<uint8_t>& annex_b) {
    size_t n = 0;
    h264::for_each_nal(annex_b.data(), annex_b.size(),
                       [&](uint8_t, const uint8_t*, size_t size) { n += 4 + size; });
    return n;
}

void to_avcc(const std::vector<uint8_t>& annex_b, std::vector<uint8_t>& out) {
    out.clear();
    h264::for_each_nal(annex_b.data(), annex_b.size(), [&](uint8_t, const uint8_t* nal, size_t size) {
        out.push_back(static_cast<uint8_t>(size >> 24));
        out.push_back(static_cast<uint8_t>(size >> 16));
        out.push_back(static_cast<uint8_t>(size >> 8));
        out.push_back(static_cast<uint8_t>(size));
        out.insert(out.end(), nal, nal + size);
    });
}

/// One run of equal values, for stts and stsc.
struct Run {
    uint32_t count = 0;
    uint32_t value = 0;
};

void add_run(std::vector<Run>& runs, uint32_t value) {
    if (!runs.empty() && runs.back().value == value) {
        ++runs.back().count;
    } else {
        runs.push_back({1, value});
    }
}

/// One track's sample table, with chunks given as offsets from the start of
/// the mdat payload.
struct Track {
    std::vector<uint32_t> sizes;
    std::vector<Run> durations;      // stts
    std::vector<uint32_t> sync;      // stss, 1-based; empty for audio (all sync)
    std::vector<uint64_t> chunk_at;  // offsets in the mdat payload
    std::vector<uint32_t> chunk_samples;
};

void write_matrix(mp4::Writer& w) {
    const uint32_t m[9] = {0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000};
    for (uint32_t v : m) w.u32(v);
}

void write_stbl_tables(mp4::Writer& w, const Track& t, uint64_t base) {
    w.begin_full("stts", 0, 0);
    w.u32(static_cast<uint32_t>(t.durations.size()));
    for (const Run& r : t.durations) {
        w.u32(r.count);
        w.u32(r.value);
    }
    w.end();
    if (!t.sync.empty()) {
        w.begin_full("stss", 0, 0);
        w.u32(static_cast<uint32_t>(t.sync.size()));
        for (uint32_t s : t.sync) w.u32(s);
        w.end();
    }
    // stsc: runs of chunks with the same sample count.
    std::vector<std::pair<uint32_t, uint32_t>> stsc;  // first chunk (1-based), samples
    for (size_t c = 0; c < t.chunk_samples.size(); ++c) {
        if (stsc.empty() || stsc.back().second != t.chunk_samples[c]) {
            stsc.emplace_back(static_cast<uint32_t>(c + 1), t.chunk_samples[c]);
        }
    }
    w.begin_full("stsc", 0, 0);
    w.u32(static_cast<uint32_t>(stsc.size()));
    for (const auto& e : stsc) {
        w.u32(e.first);
        w.u32(e.second);
        w.u32(1);
    }
    w.end();
    w.begin_full("stsz", 0, 0);
    w.u32(0);
    w.u32(static_cast<uint32_t>(t.sizes.size()));
    for (uint32_t s : t.sizes) w.u32(s);
    w.end();
    // The clip cap is 64 MiB (CLIPS.md 15.8), so 32-bit offsets fit.
    w.begin_full("stco", 0, 0);
    w.u32(static_cast<uint32_t>(t.chunk_at.size()));
    for (uint64_t at : t.chunk_at) w.u32(static_cast<uint32_t>(base + at));
    w.end();
}

void write_dinf(mp4::Writer& w) {
    w.begin("dinf");
    w.begin_full("dref", 0, 0);
    w.u32(1);
    w.begin_full("url ", 0, 1);  // the media is in this file
    w.end();
    w.end();
    w.end();
}

struct MovieInfo {
    uint32_t width = 0;
    uint32_t height = 0;
    std::vector<uint8_t> sps;
    std::vector<uint8_t> pps;
    uint64_t video_ticks = 0;  // in kVideoScale
    uint64_t audio_frames = 0; // the clip's sound, in samples, after the pre-skip
    uint32_t audio_packets = 0;
    uint16_t pre_skip = 0;
    uint32_t audio_bitrate = 0;
};

uint32_t to_movie(uint64_t value, uint32_t scale) {
    return static_cast<uint32_t>((value * kMovieScale + scale / 2) / scale);
}

std::vector<uint8_t> build_moov(const MovieInfo& m, const Track& video, const Track* audio, uint64_t base) {
    mp4::Writer w;
    const uint32_t video_ms = to_movie(m.video_ticks, kVideoScale);
    const uint32_t audio_ms = audio ? to_movie(m.audio_frames, kAudioRate) : 0;
    w.begin("moov");

    w.begin_full("mvhd", 0, 0);
    w.u32(0);  // creation time: none, so the file does not depend on the clock
    w.u32(0);
    w.u32(kMovieScale);
    w.u32(std::max(video_ms, audio_ms));
    w.u32(0x00010000);  // rate 1.0
    w.u16(0x0100);      // volume 1.0
    w.zeros(10);
    write_matrix(w);
    w.zeros(24);
    w.u32(audio ? 3 : 2);  // the next track id
    w.end();

    // ---- Video ----
    w.begin("trak");
    w.begin_full("tkhd", 0, 3);  // enabled, in the movie
    w.u32(0);
    w.u32(0);
    w.u32(1);
    w.u32(0);
    w.u32(video_ms);
    w.zeros(8);
    w.u16(0);  // layer
    w.u16(0);  // alternate group
    w.u16(0);  // volume
    w.u16(0);
    write_matrix(w);
    w.u32(m.width << 16);
    w.u32(m.height << 16);
    w.end();
    w.begin("edts");
    w.begin_full("elst", 0, 0);
    w.u32(1);
    w.u32(video_ms);
    w.u32(0);           // media time
    w.u32(0x00010000);  // rate 1.0
    w.end();
    w.end();
    w.begin("mdia");
    w.begin_full("mdhd", 0, 0);
    w.u32(0);
    w.u32(0);
    w.u32(kVideoScale);
    w.u32(static_cast<uint32_t>(m.video_ticks));
    w.u16(0x55C4);  // "und"
    w.u16(0);
    w.end();
    w.begin_full("hdlr", 0, 0);
    w.u32(0);
    w.type("vide");
    w.zeros(12);
    w.data("VideoHandler", 13);
    w.end();
    w.begin("minf");
    w.begin_full("vmhd", 0, 1);
    w.zeros(8);
    w.end();
    write_dinf(w);
    w.begin("stbl");
    w.begin_full("stsd", 0, 0);
    w.u32(1);
    w.begin("avc1");
    w.zeros(6);
    w.u16(1);  // data reference index
    w.zeros(16);
    w.u16(static_cast<uint16_t>(m.width));
    w.u16(static_cast<uint16_t>(m.height));
    w.u32(0x00480000);  // 72 dpi
    w.u32(0x00480000);
    w.u32(0);
    w.u16(1);  // frame count
    w.zeros(32);
    w.u16(0x0018);
    w.u16(0xFFFF);
    w.begin("avcC");
    w.u8(1);
    w.u8(m.sps[1]);  // profile
    w.u8(m.sps[2]);  // constraint flags
    w.u8(m.sps[3]);  // level
    w.u8(0xFF);      // 4-byte NAL lengths
    w.u8(0xE1);      // one SPS
    w.u16(static_cast<uint16_t>(m.sps.size()));
    w.data(m.sps.data(), m.sps.size());
    w.u8(1);  // one PPS
    w.u16(static_cast<uint16_t>(m.pps.size()));
    w.data(m.pps.data(), m.pps.size());
    // High profiles carry the chroma format and bit depths (ISO 14496-15
    // 5.3.3.1). The recording encoders make 4:2:0 at 8 bits.
    const uint8_t profile = m.sps[1];
    if (profile == 100 || profile == 110 || profile == 122 || profile == 144) {
        w.u8(0xFC | 1);
        w.u8(0xF8);
        w.u8(0xF8);
        w.u8(0);
    }
    w.end();
    w.begin("pasp");
    w.u32(1);
    w.u32(1);
    w.end();
    w.end();  // avc1
    w.end();  // stsd
    write_stbl_tables(w, video, base);
    w.end();  // stbl
    w.end();  // minf
    w.end();  // mdia
    w.end();  // trak

    // ---- Sound ----
    if (audio) {
        w.begin("trak");
        w.begin_full("tkhd", 0, 3);
        w.u32(0);
        w.u32(0);
        w.u32(2);
        w.u32(0);
        w.u32(audio_ms);
        w.zeros(8);
        w.u16(0);
        w.u16(1);       // alternate group
        w.u16(0x0100);  // volume 1.0
        w.u16(0);
        write_matrix(w);
        w.u32(0);
        w.u32(0);
        w.end();
        // The first heard sample is the clip's T0: the edit skips the
        // encoder's pre-skip (the Opus encapsulation spec, 4.4).
        w.begin("edts");
        w.begin_full("elst", 0, 0);
        w.u32(1);
        w.u32(audio_ms);
        w.u32(m.pre_skip);
        w.u32(0x00010000);
        w.end();
        w.end();
        w.begin("mdia");
        w.begin_full("mdhd", 0, 0);
        w.u32(0);
        w.u32(0);
        w.u32(kAudioRate);
        w.u32(m.audio_packets * kOpusFrame);
        w.u16(0x55C4);
        w.u16(0);
        w.end();
        w.begin_full("hdlr", 0, 0);
        w.u32(0);
        w.type("soun");
        w.zeros(12);
        w.data("SoundHandler", 13);
        w.end();
        w.begin("minf");
        w.begin_full("smhd", 0, 0);
        w.zeros(4);
        w.end();
        write_dinf(w);
        w.begin("stbl");
        w.begin_full("stsd", 0, 0);
        w.u32(1);
        w.begin("Opus");
        w.zeros(6);
        w.u16(1);
        w.zeros(8);
        w.u16(2);   // channels
        w.u16(16);  // sample size
        w.u16(0);
        w.u16(0);
        w.u32(kAudioRate << 16);
        w.begin("dOps");
        w.u8(0);  // version
        w.u8(2);  // output channels
        w.u16(m.pre_skip);
        w.u32(kAudioRate);  // input rate
        w.u16(0);           // output gain
        w.u8(0);            // mapping family 0: mono or stereo
        w.end();
        w.begin("btrt");
        w.u32(0);
        w.u32(m.audio_bitrate);
        w.u32(m.audio_bitrate);
        w.end();
        w.end();  // Opus
        w.end();  // stsd
        write_stbl_tables(w, *audio, base);
        // The roll group: seeking to a sample needs the 4 frames before it.
        // The first 4 samples have nothing before them.
        w.begin_full("sgpd", 1, 0);
        w.type("roll");
        w.u32(2);  // default length
        w.u32(1);
        w.u16(static_cast<uint16_t>(kOpusRoll));
        w.end();
        const uint32_t head = std::min<uint32_t>(4, m.audio_packets);
        w.begin_full("sbgp", 0, 0);
        w.type("roll");
        w.u32(m.audio_packets > head ? 2 : 1);
        w.u32(head);
        w.u32(0);
        if (m.audio_packets > head) {
            w.u32(m.audio_packets - head);
            w.u32(1);
        }
        w.end();
        w.end();  // stbl
        w.end();  // minf
        w.end();  // mdia
        w.end();  // trak
    }

    w.end();  // moov
    return w.bytes();
}

std::vector<uint8_t> build_ftyp() {
    mp4::Writer w;
    w.begin("ftyp");
    w.type("isom");
    w.u32(0x200);
    w.type("isom");
    w.type("iso2");
    w.type("avc1");
    w.type("mp41");
    w.end();
    return w.bytes();
}

/// The clip mix as Opus packets: 20 ms each, the encoder's look-ahead as the
/// pre-skip. The input is padded with silence so the last sample of the mix
/// leaves the encoder.
bool encode_opus(const std::vector<int16_t>& stereo, uint32_t bitrate,
                 std::vector<std::vector<uint8_t>>& packets, uint16_t& pre_skip) {
    int err = 0;
    std::unique_ptr<OpusEncoder, OpusEncoderDeleter> enc(
        opus_encoder_create(kAudioRate, 2, OPUS_APPLICATION_AUDIO, &err));
    if (!enc || err != OPUS_OK) {
        MELLO_LOG_ERROR(TAG, "mux: opus_encoder_create failed: %d", err);
        return false;
    }
    opus_encoder_ctl(enc.get(), OPUS_SET_BITRATE(static_cast<opus_int32>(bitrate)));
    opus_encoder_ctl(enc.get(), OPUS_SET_VBR(1));
    opus_encoder_ctl(enc.get(), OPUS_SET_COMPLEXITY(10));
    opus_encoder_ctl(enc.get(), OPUS_SET_SIGNAL(OPUS_SIGNAL_MUSIC));
    opus_int32 lookahead = 0;
    opus_encoder_ctl(enc.get(), OPUS_GET_LOOKAHEAD(&lookahead));
    pre_skip = static_cast<uint16_t>(std::max<opus_int32>(lookahead, 0));

    const size_t frames = stereo.size() / 2;
    const size_t total = frames + pre_skip;
    const size_t count = (total + kOpusFrame - 1) / kOpusFrame;
    std::vector<int16_t> pcm(kOpusFrame * 2);
    std::vector<uint8_t> out(4000);
    packets.clear();
    packets.reserve(count);
    for (size_t p = 0; p < count; ++p) {
        const size_t from = p * kOpusFrame;
        const size_t have = from < frames ? std::min<size_t>(kOpusFrame, frames - from) : 0;
        std::fill(pcm.begin(), pcm.end(), int16_t{0});
        if (have > 0) std::copy_n(stereo.begin() + static_cast<std::ptrdiff_t>(from * 2), have * 2, pcm.begin());
        const int n = opus_encode(enc.get(), pcm.data(), kOpusFrame, out.data(), static_cast<opus_int32>(out.size()));
        if (n < 0) {
            MELLO_LOG_ERROR(TAG, "mux: opus_encode failed: %d", n);
            return false;
        }
        packets.emplace_back(out.begin(), out.begin() + n);
    }
    return true;
}

}  // namespace

bool write_clip_mp4(const ClipMuxInput& in, const std::filesystem::path& path, ClipMuxResult* result) {
    if (in.units.empty() || !in.read_unit || in.width == 0 || in.height == 0 || in.width > 0xFFFF ||
        in.height > 0xFFFF) {
        MELLO_LOG_ERROR(TAG, "mux: no video");
        return false;
    }
    if (!in.units.front().keyframe) {
        MELLO_LOG_ERROR(TAG, "mux: the first unit is not a keyframe");
        return false;
    }
    std::vector<uint8_t> bytes;
    if (!in.read_unit(in.units.front(), bytes)) {
        MELLO_LOG_ERROR(TAG, "mux: cannot read the first unit");
        return false;
    }
    MovieInfo movie;
    movie.width = in.width;
    movie.height = in.height;
    h264::for_each_nal(bytes.data(), bytes.size(), [&](uint8_t type, const uint8_t* nal, size_t size) {
        if (type == h264::kNalSps && movie.sps.empty()) movie.sps.assign(nal, nal + size);
        if (type == h264::kNalPps && movie.pps.empty()) movie.pps.assign(nal, nal + size);
    });
    if (movie.sps.size() < 4 || movie.pps.empty()) {
        MELLO_LOG_ERROR(TAG, "mux: the first unit has no SPS and PPS");
        return false;
    }

    // The video times: each frame at its capture time from the first one,
    // lasting until the next. A time that does not move forward (it cannot,
    // the ring keeps capture order) moves 0.1 ms on.
    const uint64_t t0 = in.units.front().capture_us;
    const size_t n = in.units.size();
    std::vector<uint64_t> at_us(n);
    for (size_t i = 0; i < n; ++i) {
        at_us[i] = in.units[i].capture_us >= t0 ? in.units[i].capture_us - t0 : 0;
        if (i > 0 && at_us[i] <= at_us[i - 1]) at_us[i] = at_us[i - 1] + 100;
    }
    const uint64_t nominal_us = 1'000'000 / std::max<uint32_t>(in.fps, 1);
    const uint64_t end_rel = in.end_us > t0 ? in.end_us - t0 : 0;
    // The last frame lasts until the end of the clip, else one nominal frame.
    const uint64_t video_end_us = end_rel > at_us[n - 1] ? end_rel : at_us[n - 1] + nominal_us;
    auto ticks = [](uint64_t us) { return (us * kVideoScale + 500'000) / 1'000'000; };

    // Pass 1: the size of each unit as stored (AVCC) and the video table.
    Track video;
    video.sizes.resize(n);
    for (size_t i = 0; i < n; ++i) {
        if (i > 0 && !in.read_unit(in.units[i], bytes)) {
            MELLO_LOG_ERROR(TAG, "mux: cannot read unit %zu", i);
            return false;
        }
        video.sizes[i] = static_cast<uint32_t>(avcc_size(bytes));
        if (video.sizes[i] == 0) {
            MELLO_LOG_ERROR(TAG, "mux: unit %zu is not Annex B", i);
            return false;
        }
        if (in.units[i].keyframe) video.sync.push_back(static_cast<uint32_t>(i + 1));
        const uint64_t next = i + 1 < n ? at_us[i + 1] : video_end_us;
        const uint64_t d = ticks(next) > ticks(at_us[i]) ? ticks(next) - ticks(at_us[i]) : 1;
        add_run(video.durations, static_cast<uint32_t>(d));
        movie.video_ticks += d;
    }

    // The sound: Opus, encoded before the write (30 s is about 600 KB).
    std::vector<std::vector<uint8_t>> packets;
    const bool has_audio = !in.audio.empty();
    Track audio;
    if (has_audio) {
        if (!encode_opus(in.audio, in.audio_bitrate, packets, movie.pre_skip)) return false;
        movie.audio_frames = in.audio.size() / 2;
        movie.audio_packets = static_cast<uint32_t>(packets.size());
        movie.audio_bitrate = in.audio_bitrate;
        audio.sizes.reserve(packets.size());
        for (const auto& p : packets) audio.sizes.push_back(static_cast<uint32_t>(p.size()));
        audio.durations.push_back({movie.audio_packets, kOpusFrame});
    }

    // The chunks: 0.5 s of video, then the sound of the same 0.5 s. A sound
    // packet's time is its first heard sample (after the pre-skip).
    struct Chunk {
        bool is_video = false;
        size_t first = 0;
        size_t count = 0;
    };
    std::vector<Chunk> chunks;
    uint64_t payload = 0;
    size_t vi = 0;
    size_t ai = 0;
    for (uint64_t window_end = kChunkUs; vi < n || ai < packets.size(); window_end += kChunkUs) {
        Chunk v{true, vi, 0};
        while (vi < n && at_us[vi] < window_end) {
            ++vi;
            ++v.count;
        }
        if (v.count > 0) {
            video.chunk_at.push_back(payload);
            video.chunk_samples.push_back(static_cast<uint32_t>(v.count));
            for (size_t i = v.first; i < v.first + v.count; ++i) payload += video.sizes[i];
            chunks.push_back(v);
        }
        Chunk a{false, ai, 0};
        while (ai < packets.size()) {
            const int64_t heard = static_cast<int64_t>(ai) * kOpusFrame - movie.pre_skip;
            const int64_t heard_us = heard * 1'000'000 / static_cast<int64_t>(kAudioRate);
            if (heard_us >= static_cast<int64_t>(window_end)) break;
            ++ai;
            ++a.count;
        }
        if (a.count > 0) {
            audio.chunk_at.push_back(payload);
            audio.chunk_samples.push_back(static_cast<uint32_t>(a.count));
            for (size_t i = a.first; i < a.first + a.count; ++i) payload += audio.sizes[i];
            chunks.push_back(a);
        }
    }

    // moov before mdat: its size does not depend on the offsets in it.
    const std::vector<uint8_t> ftyp = build_ftyp();
    const size_t moov_size = build_moov(movie, video, has_audio ? &audio : nullptr, 0).size();
    const uint64_t mdat_header = 8;
    const uint64_t base = ftyp.size() + moov_size + mdat_header;
    if (base + payload > 0xFFFFFFFFull) {
        MELLO_LOG_ERROR(TAG, "mux: the clip is over 4 GB");
        return false;
    }
    const std::vector<uint8_t> moov = build_moov(movie, video, has_audio ? &audio : nullptr, base);

    std::error_code ec;
    std::filesystem::path part = path;
    part += ".part";
    std::filesystem::remove(part, ec);
    {
        std::ofstream out(part, std::ios::binary | std::ios::trunc);
        if (!out) {
            MELLO_LOG_ERROR(TAG, "mux: cannot open %s", part.string().c_str());
            return false;
        }
        out.write(reinterpret_cast<const char*>(ftyp.data()), static_cast<std::streamsize>(ftyp.size()));
        out.write(reinterpret_cast<const char*>(moov.data()), static_cast<std::streamsize>(moov.size()));
        mp4::Writer mdat;
        mdat.u32(static_cast<uint32_t>(mdat_header + payload));
        mdat.type("mdat");
        out.write(reinterpret_cast<const char*>(mdat.bytes().data()), 8);

        // Pass 2: the media, one unit at a time. The file is never in RAM.
        std::vector<uint8_t> avcc;
        bool ok = true;
        for (const Chunk& c : chunks) {
            for (size_t i = c.first; ok && i < c.first + c.count; ++i) {
                if (c.is_video) {
                    if (!in.read_unit(in.units[i], bytes)) {
                        MELLO_LOG_ERROR(TAG, "mux: cannot read unit %zu again", i);
                        ok = false;
                        break;
                    }
                    to_avcc(bytes, avcc);
                    if (avcc.size() != video.sizes[i]) {
                        MELLO_LOG_ERROR(TAG, "mux: unit %zu changed between the passes", i);
                        ok = false;
                        break;
                    }
                    out.write(reinterpret_cast<const char*>(avcc.data()), static_cast<std::streamsize>(avcc.size()));
                } else {
                    out.write(reinterpret_cast<const char*>(packets[i].data()),
                              static_cast<std::streamsize>(packets[i].size()));
                }
            }
            if (!ok) break;
        }
        out.flush();
        if (!ok || !out.good()) {
            if (ok) MELLO_LOG_ERROR(TAG, "mux: the write failed (disk full?)");
            out.close();
            std::filesystem::remove(part, ec);
            return false;
        }
    }
    std::filesystem::rename(part, path, ec);
    if (ec) {
        MELLO_LOG_ERROR(TAG, "mux: rename to %s failed: %s", path.string().c_str(), ec.message().c_str());
        std::filesystem::remove(part, ec);
        return false;
    }

    const uint64_t file_bytes = base + payload;
    if (result) {
        result->duration_us = movie.video_ticks * 1'000'000 / kVideoScale;
        result->video_frames = static_cast<uint32_t>(n);
        result->bytes = file_bytes;
    }
    MELLO_LOG_INFO(TAG, "mux: %s, %zu frames, %.2f s, %llu bytes, sound %s", path.string().c_str(), n,
                   static_cast<double>(movie.video_ticks) / kVideoScale,
                   static_cast<unsigned long long>(file_bytes),
                   has_audio ? (std::to_string(packets.size()) + " Opus packets").c_str() : "none");
    return true;
}

}  // namespace mello::replay
