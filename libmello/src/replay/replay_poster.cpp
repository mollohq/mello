// The poster from a game clip MP4 (CLIPS.md 15.5), the same on every
// platform: m3llo's demux finds the key frame, the stream viewer's decoder
// decodes it, and the platform's JPEG writer (write_poster_jpeg) stores it.
// The poster when no frame came after the press: a minimized game.

#include "replay_mux.hpp"
#include "mp4_demux.hpp"
#include "../util/log.hpp"
#include "../video/default_device.hpp"
#include "../video/video_viewer.hpp"

#include <chrono>
#include <condition_variable>
#include <mutex>
#include <string>
#include <vector>

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/poster";
/// Units fed after the key frame before the poster gives up: a decoder may
/// hold a frame or two before it gives the first one.
constexpr size_t kMaxFeed = 8;
constexpr auto kFrameWait = std::chrono::milliseconds(500);

}  // namespace

bool write_poster_from_mp4(const std::filesystem::path& mp4, uint64_t time_us, const std::filesystem::path& jpeg) {
    uint64_t size = 0;
    const Mp4ReadAt read = file_reader(mp4, &size);
    Mp4Movie movie;
    std::string error;
    if (size == 0 || !open_mp4(read, size, movie, &error)) {
        MELLO_LOG_ERROR(TAG, "poster: %s does not open: %s", mp4.string().c_str(), error.c_str());
        return false;
    }
    const Mp4Track* video = movie.video();
    if (!video || video->samples.empty()) {
        MELLO_LOG_ERROR(TAG, "poster: %s has no H.264 video", mp4.string().c_str());
        return false;
    }
    // The key frame at or before the time.
    size_t key = 0;
    for (size_t i = 0; i < video->samples.size(); ++i) {
        const Mp4Sample& s = video->samples[i];
        if (s.sync && video->time_us(s.time) <= static_cast<int64_t>(time_us)) key = i;
    }

    std::mutex mutex;
    std::condition_variable cv;
    std::vector<uint8_t> bgra;
    uint32_t width = 0, height = 0;
    bool have = false;
    video::GraphicsDevice device = video::create_viewer_device();
    if (!device.handle) {
        MELLO_LOG_ERROR(TAG, "poster: no graphics device");
        return false;
    }
    bool ok = false;
    {
        video::ViewerPipeline viewer;
        video::ViewerConfig config;
        config.width = video->width;
        config.height = video->height;
        config.fps = 60;
        const bool started =
            viewer.start(device, config, [&](const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t) {
                std::lock_guard<std::mutex> lock(mutex);
                if (have) return;
                // The viewer gives RGBA; the JPEG writer takes BGRA.
                bgra.resize(static_cast<size_t>(w) * h * 4);
                for (size_t p = 0; p < bgra.size(); p += 4) {
                    bgra[p] = rgba[p + 2];
                    bgra[p + 1] = rgba[p + 1];
                    bgra[p + 2] = rgba[p];
                    bgra[p + 3] = 255;
                }
                width = w;
                height = h;
                have = true;
                cv.notify_all();
            });
        if (!started) {
            MELLO_LOG_ERROR(TAG, "poster: no decoder for %ux%u", video->width, video->height);
        } else {
            static const uint8_t kStart[] = {0, 0, 0, 1};
            std::vector<uint8_t> raw;
            std::vector<uint8_t> unit;
            for (size_t i = key; i < video->samples.size() && i < key + kMaxFeed; ++i) {
                const Mp4Sample& s = video->samples[i];
                if (!read_sample(read, s, raw) || !avcc_to_annex_b(raw.data(), raw.size(), video->nal_length_size, unit)) {
                    break;
                }
                if (i == key) {
                    // The track's parameter sets first, in case the file keeps
                    // them only in the track header.
                    std::vector<uint8_t> header;
                    for (const auto* sets : {&video->sps, &video->pps}) {
                        for (const auto& p : *sets) {
                            header.insert(header.end(), kStart, kStart + 4);
                            header.insert(header.end(), p.begin(), p.end());
                        }
                    }
                    unit.insert(unit.begin(), header.begin(), header.end());
                }
                viewer.feed_packet(unit.data(), unit.size(), s.sync);
                std::unique_lock<std::mutex> lock(mutex);
                if (cv.wait_for(lock, kFrameWait, [&] { return have; })) break;
            }
            std::vector<uint8_t> image;
            {
                std::lock_guard<std::mutex> lock(mutex);
                image = bgra;
            }
            viewer.stop();
            if (image.empty()) {
                MELLO_LOG_ERROR(TAG, "poster: no frame at %.2f s in %s", static_cast<double>(time_us) / 1e6,
                                mp4.string().c_str());
            } else {
                ok = write_poster_jpeg(image.data(), width, height, width * 4, jpeg);
            }
        }
    }
    video::release_viewer_device(device);
    return ok;
}

#if !defined(_WIN32) && (!defined(__APPLE__) || defined(MELLO_IOS))
bool write_poster_jpeg(const uint8_t*, uint32_t, uint32_t, uint32_t, const std::filesystem::path&) {
    // No JPEG writer on this platform (Linux, iOS): it records no game clips.
    MELLO_LOG_WARN(TAG, "poster: no JPEG writer on this platform");
    return false;
}
#endif

}  // namespace mello::replay
