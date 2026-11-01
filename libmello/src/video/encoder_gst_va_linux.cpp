#if defined(__linux__)
#include "encoder_gst_va.hpp"
#include "gst_runtime_linux.hpp"
#include "../util/log.hpp"

#include <algorithm>
#include <cstring>
#include <string>

namespace mello::video {

static constexpr const char* TAG = "video/encoder";

// How long encode() waits for the access unit of the picture it pushed. A
// later unit comes out of the next call; none is dropped.
static constexpr GstClockTime kPullTimeout = 50 * GST_MSECOND;

struct GstVaEncoder::Pipeline {
    GstElement* pipeline = nullptr;
    GstElement* src = nullptr;
    GstElement* enc = nullptr;
    GstElement* sink = nullptr;
    GstBus* bus = nullptr;
    uint32_t src_w = 0;  // the caps appsrc announces now
    uint32_t src_h = 0;
};

GstVaEncoder::GstVaEncoder() = default;

GstVaEncoder::~GstVaEncoder() {
    shutdown();
}

bool GstVaEncoder::is_available() {
    return !gst::va_element("h264enc").empty() && !gst::va_element("postproc").empty();
}

static std::string src_caps(uint32_t w, uint32_t h) {
    return "video/x-raw,format=BGRx,width=" + std::to_string(w) + ",height=" + std::to_string(h) +
           ",framerate=0/1";
}

bool GstVaEncoder::initialize(const GraphicsDevice& device, const EncoderConfig& config) {
    (void)device;
    auto fail = [this](const char* why) {
        MELLO_LOG_INFO(TAG, "VA-API encoder not used: %s", why);
        shutdown();
        return false;
    };
    if (config.codec != VideoCodec::H264) return fail("not H.264");
    if (config.width == 0 || config.height == 0 || (config.width & 1) || (config.height & 1)) {
        return fail("invalid size");
    }
    const std::string enc = gst::va_element("h264enc");
    const std::string post = gst::va_element("postproc");
    if (enc.empty() || post.empty()) return fail("no VA H.264 encoder or post-processor (gst-plugin-va)");
    config_ = config;

    const std::string w = std::to_string(config.width), h = std::to_string(config.height);
    const std::string kbps = std::to_string(config.bitrate_kbps);
    const std::string gop = std::to_string(config.keyframe_interval);
    const std::string head = "appsrc name=src is-live=true format=time do-timestamp=false caps=" +
                             src_caps(config.width, config.height) + " ! " + post + " ! ";
    const std::string tail = " ! video/x-h264,profile=high,stream-format=byte-stream,alignment=au"
                             " ! appsink name=sink sync=false max-buffers=16 drop=false";
    // Full settings first. A GStreamer without one of the properties, or a
    // driver that refuses BT.709 in the caps, gets the next, simpler line.
    const std::string full = head + "video/x-raw(memory:VAMemory),format=NV12,width=" + w + ",height=" + h +
                             ",colorimetry=bt709 ! " + enc + " name=enc rate-control=vbr bitrate=" + kbps +
                             " target-percentage=80 key-int-max=" + gop +
                             " b-frames=0 ref-frames=1 target-usage=7" + tail;
    const std::string plain = head + "video/x-raw(memory:VAMemory),format=NV12,width=" + w + ",height=" + h +
                              " ! " + enc + " name=enc bitrate=" + kbps + " key-int-max=" + gop +
                              " b-frames=0" + tail;

    const gst::Api& a = gst::api();
    p_ = std::make_unique<Pipeline>();
    for (const std::string* desc : {&full, &plain}) {
        GError* err = nullptr;
        GstElement* pipeline = a.parse_launch(desc->c_str(), &err);
        if (err) {
            MELLO_LOG_INFO(TAG, "VA-API encoder: pipeline refused: %s", err->message);
            a.error_free(err);
            if (pipeline) a.object_unref(pipeline);
            continue;
        }
        p_->pipeline = pipeline;
        p_->src = a.bin_get_by_name(reinterpret_cast<GstBin*>(pipeline), "src");
        p_->enc = a.bin_get_by_name(reinterpret_cast<GstBin*>(pipeline), "enc");
        p_->sink = a.bin_get_by_name(reinterpret_cast<GstBin*>(pipeline), "sink");
        p_->bus = a.get_bus(pipeline);
        if (p_->src && p_->enc && p_->sink && p_->bus &&
            a.set_state(pipeline, GST_STATE_PLAYING) != GST_STATE_CHANGE_FAILURE) {
            break;
        }
        MELLO_LOG_INFO(TAG, "VA-API encoder: pipeline does not start, trying the simpler one");
        shutdown();
        p_ = std::make_unique<Pipeline>();
    }
    if (!p_->pipeline) return fail("no pipeline");
    p_->src_w = config.width;
    p_->src_h = config.height;
    stats_ = {};
    stats_window_start_ = {};
    backlog_.clear();
    first_pts_us_ = 0;
    failed_ = false;
    MELLO_LOG_INFO(TAG, "Selected encoder: VA-API (GStreamer %s, %s) %ux%u %u fps %u kbps idr_interval=%u",
                   enc.c_str(), post.c_str(), config.width, config.height, config.fps, config.bitrate_kbps,
                   config.keyframe_interval);
    return true;
}

void GstVaEncoder::shutdown() {
    if (!p_) return;
    const gst::Api& a = gst::api();
    if (p_->pipeline) a.set_state(p_->pipeline, GST_STATE_NULL);
    for (GstElement* e : {p_->src, p_->enc, p_->sink}) {
        if (e) a.object_unref(e);
    }
    if (p_->bus) a.object_unref(p_->bus);
    if (p_->pipeline) a.object_unref(p_->pipeline);
    p_.reset();
    backlog_.clear();
}

bool GstVaEncoder::encode(const CpuFrame& frame, uint64_t capture_us, EncodedPacket& out) {
    if (!p_ || failed_ || !frame.data || frame.width < 2 || frame.height < 2) return false;
    const gst::Api& a = gst::api();
    const auto t0 = std::chrono::steady_clock::now();

    // Controls from other threads, before this picture.
    const uint32_t kbps = pending_bitrate_kbps_.exchange(0, std::memory_order_relaxed);
    if (kbps > 0 && kbps != config_.bitrate_kbps) {
        a.object_set(p_->enc, "bitrate", static_cast<guint>(kbps), nullptr);
        config_.bitrate_kbps = kbps;
    }
    if (force_idr_.exchange(false, std::memory_order_relaxed)) {
        GstEvent* ev = a.force_key_unit_event(GST_CLOCK_TIME_NONE, GST_CLOCK_TIME_NONE, GST_CLOCK_TIME_NONE,
                                              TRUE, 0);
        a.send_event(p_->enc, ev);
    }

    // A new source size (a resized window): announce it, vapostproc scales.
    if (frame.width != p_->src_w || frame.height != p_->src_h) {
        GstCaps* caps = a.caps_from_string(src_caps(frame.width, frame.height).c_str());
        a.object_set(p_->src, "caps", caps, nullptr);
        a.mini_object_unref(GST_MINI_OBJECT_CAST(caps));
        p_->src_w = frame.width;
        p_->src_h = frame.height;
        MELLO_LOG_INFO(TAG, "VA-API encoder: source is now %ux%u", frame.width, frame.height);
    }

    const size_t row = static_cast<size_t>(frame.width) * 4;
    GstBuffer* buf = a.buffer_new_allocate(nullptr, row * frame.height, nullptr);
    if (!buf) return false;
    if (frame.stride == row) {
        a.buffer_fill(buf, 0, frame.data, row * frame.height);
    } else {
        GstMapInfo map;
        std::memset(&map, 0, sizeof(map));
        if (!a.buffer_map(buf, &map, GST_MAP_WRITE)) {
            a.mini_object_unref(GST_MINI_OBJECT_CAST(buf));
            return false;
        }
        for (uint32_t y = 0; y < frame.height; ++y) {
            std::memcpy(map.data + y * row, frame.data + static_cast<size_t>(y) * frame.stride, row);
        }
        a.buffer_unmap(buf, &map);
    }
    if (first_pts_us_ == 0) first_pts_us_ = capture_us;
    GST_BUFFER_PTS(buf) = (capture_us - first_pts_us_) * GST_USECOND;
    GST_BUFFER_DTS(buf) = GST_BUFFER_PTS(buf);
    const auto t1 = std::chrono::steady_clock::now();
    if (a.app_src_push_buffer(reinterpret_cast<GstAppSrc*>(p_->src), buf) != GST_FLOW_OK) {
        MELLO_LOG_ERROR(TAG, "VA-API encoder: appsrc refused the picture");
        failed_ = true;
        return false;
    }

    // Every access unit that is ready, in order.
    GstClockTime wait = kPullTimeout;
    while (GstSample* sample = a.app_sink_try_pull_sample(reinterpret_cast<GstAppSink*>(p_->sink), wait)) {
        wait = 0;
        GstBuffer* au = a.sample_get_buffer(sample);
        GstMapInfo map;
        std::memset(&map, 0, sizeof(map));
        if (au && a.buffer_map(au, &map, GST_MAP_READ)) {
            EncodedPacket packet;
            packet.data.assign(map.data, map.data + map.size);
            packet.is_keyframe = !GST_BUFFER_FLAG_IS_SET(au, GST_BUFFER_FLAG_DELTA_UNIT);
            packet.timestamp_us = GST_BUFFER_PTS_IS_VALID(au) ? first_pts_us_ + GST_BUFFER_PTS(au) / GST_USECOND
                                                              : capture_us;
            a.buffer_unmap(au, &map);
            if (!packet.data.empty()) backlog_.push_back(std::move(packet));
        }
        a.mini_object_unref(GST_MINI_OBJECT_CAST(sample));
    }
    const auto t2 = std::chrono::steady_clock::now();
    if (gst::pop_bus_error(p_->bus, "VA-API encoder")) {
        // A pipeline in error stops for good.
        failed_ = true;
        return false;
    }
    phases_.lock_ms = std::chrono::duration<double, std::milli>(t1 - t0).count();
    phases_.submit_ms = 0.0;
    phases_.wait_ms = std::chrono::duration<double, std::milli>(t2 - t1).count();
    if (backlog_.empty()) return false;

    out = std::move(backlog_.front());
    backlog_.pop_front();
    stats_.bytes_sent += out.data.size();
    if (out.is_keyframe) stats_.keyframes_sent++;
    stats_window_frames_++;
    stats_window_bytes_ += out.data.size();
    if (stats_window_start_.time_since_epoch().count() == 0) stats_window_start_ = t2;
    const auto elapsed_us = std::chrono::duration_cast<std::chrono::microseconds>(t2 - stats_window_start_).count();
    if (elapsed_us >= 1'000'000) {
        stats_.fps_actual = static_cast<uint32_t>((static_cast<uint64_t>(stats_window_frames_) * 1'000'000) /
                                                  static_cast<uint64_t>(elapsed_us));
        stats_.bitrate_kbps = static_cast<uint32_t>((stats_window_bytes_ * 8) / static_cast<uint64_t>(elapsed_us / 1'000));
        stats_window_frames_ = 0;
        stats_window_bytes_ = 0;
        stats_window_start_ = t2;
    }
    return true;
}

void GstVaEncoder::request_keyframe() {
    force_idr_.store(true, std::memory_order_relaxed);
}

void GstVaEncoder::set_bitrate(uint32_t kbps) {
    if (kbps > 0) pending_bitrate_kbps_.store(kbps, std::memory_order_relaxed);
}

void GstVaEncoder::get_stats(EncoderStats& out) const {
    out = stats_;
}

void GstVaEncoder::get_phase_timing(EncodePhaseTiming& out) const {
    out = phases_;
}

} // namespace mello::video
#endif
