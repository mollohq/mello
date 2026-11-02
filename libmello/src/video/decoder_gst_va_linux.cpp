#if defined(__linux__)
#include "decoder_gst_va.hpp"
#include "linux_gpu.hpp"
#include "../util/log.hpp"

#include <dlfcn.h>
#include <chrono>
#include <cstring>
#include <mutex>
#include <string>

#include "gst_runtime_linux.hpp"

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

// Waits this long for the picture of an access unit before decode() returns
// Accepted. A late picture comes out of the next call.
static constexpr GstClockTime kPullTimeout = 20 * GST_MSECOND;


struct GstVaDecoder::Pipeline {
    GstElement* pipeline = nullptr;
    GstElement* src = nullptr;
    GstElement* sink = nullptr;
    GstBus* bus = nullptr;
    std::string element;
};

GstVaDecoder::GstVaDecoder() = default;

GstVaDecoder::~GstVaDecoder() {
    shutdown();
}

bool GstVaDecoder::is_available() {
    return !gst::va_element("h264dec").empty() && gst::has_factory("h264parse");
}

void GstVaDecoder::warm_up() {
    const auto t0 = std::chrono::steady_clock::now();
    const std::string& element = gst::va_element("h264dec");
    const auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - t0);
    MELLO_LOG_INFO(TAG, "VA-API warm-up: %s in %lld ms", element.empty() ? "no VA H.264 decoder" : element.c_str(),
                   static_cast<long long>(ms.count()));
}

bool GstVaDecoder::initialize(const GraphicsDevice& device, const DecoderConfig& config) {
    (void)device;
    auto fail = [this](const char* why) {
        MELLO_LOG_INFO(TAG, "VA-API not used: %s", why);
        shutdown();
        return false;
    };
    if (config.codec != VideoCodec::H264) return fail("not H.264");
    const auto t0 = std::chrono::steady_clock::now();
    if (!gst::load()) return fail("GStreamer not found");
    if (!gst::has_factory("h264parse")) return fail("no h264parse (gst-plugins-bad)");
    const std::string element = gst::va_element("h264dec");
    if (element.empty()) return fail("no VA H.264 decoder (gst-plugin-va)");
    const auto t1 = std::chrono::steady_clock::now();

    // compliance=flexible: show each picture as soon as it is decoded. Our
    // hosts send no reordered frames, and a live viewer must not wait for
    // the DPB to fill. A GStreamer older than 1.20 lacks the property.
    const char* caps = "video/x-h264,stream-format=byte-stream,alignment=au";
    std::string with = std::string("appsrc name=src is-live=true format=time caps=") + caps +
                       " ! h264parse ! " + element + " compliance=flexible" +
                       " ! video/x-raw,format=NV12 ! appsink name=sink sync=false max-buffers=3 drop=true";
    std::string without = with;
    without.erase(without.find(" compliance=flexible"), std::strlen(" compliance=flexible"));

    p_ = std::make_unique<Pipeline>();
    p_->element = element;
    for (const std::string* desc : {&with, &without}) {
        GError* err = nullptr;
        GstElement* pipeline = gst::api().parse_launch(desc->c_str(), &err);
        if (err) {
            MELLO_LOG_INFO(TAG, "VA-API: pipeline refused: %s", err->message);
            gst::api().error_free(err);
            if (pipeline) gst::api().object_unref(pipeline);
            continue;
        }
        p_->pipeline = pipeline;
        break;
    }
    if (!p_->pipeline) return fail("no pipeline");
    p_->src = gst::api().bin_get_by_name(reinterpret_cast<GstBin*>(p_->pipeline), "src");
    p_->sink = gst::api().bin_get_by_name(reinterpret_cast<GstBin*>(p_->pipeline), "sink");
    p_->bus = gst::api().get_bus(p_->pipeline);
    if (!p_->src || !p_->sink || !p_->bus) return fail("pipeline without its source or sink");

    if (gst::api().set_state(p_->pipeline, GST_STATE_PLAYING) == GST_STATE_CHANGE_FAILURE) {
        return fail("the pipeline does not start");
    }
    const auto t2 = std::chrono::steady_clock::now();
    auto ms = [](auto d) { return static_cast<long long>(std::chrono::duration_cast<std::chrono::milliseconds>(d).count()); };
    MELLO_LOG_INFO(TAG, "Selected decoder: VA-API (GStreamer %s) codec=H264 resolution=%ux%u "
                        "(GStreamer and element %lld ms, pipeline %lld ms)",
                   element.c_str(), config.width, config.height, ms(t1 - t0), ms(t2 - t1));
    return true;
}

void GstVaDecoder::shutdown() {
    if (p_) {
        if (p_->pipeline) gst::api().set_state(p_->pipeline, GST_STATE_NULL);
        if (p_->bus) gst::api().object_unref(p_->bus);
        if (p_->src) gst::api().object_unref(p_->src);
        if (p_->sink) gst::api().object_unref(p_->sink);
        if (p_->pipeline) gst::api().object_unref(p_->pipeline);
        p_.reset();
    }
    for (Slot& slot : slots_) slot = Slot{};
    frames_in_ = 0;
    failed_ = false;
}

DecodeFeedResult GstVaDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe, int slot_index) {
    (void)is_keyframe;
    if (!p_ || failed_ || slot_index < 0 || slot_index >= kOutputSlots) return DecodeFeedResult::Error;
    const gst::Api& a = gst::api();

    GstBuffer* buf = a.buffer_new_allocate(nullptr, size, nullptr);
    if (!buf) return DecodeFeedResult::Error;
    a.buffer_fill(buf, 0, data, size);
    // Steady 60 fps time stamps: the sink does not sync, they only order.
    GST_BUFFER_PTS(buf) = frames_in_ * (GST_SECOND / 60);
    GST_BUFFER_DTS(buf) = GST_BUFFER_PTS(buf);
    ++frames_in_;
    if (a.app_src_push_buffer(reinterpret_cast<GstAppSrc*>(p_->src), buf) != GST_FLOW_OK) {
        MELLO_LOG_ERROR(TAG, "VA-API: appsrc refused the access unit");
        failed_ = true;
        return DecodeFeedResult::Error;
    }

    GstSample* sample = a.app_sink_try_pull_sample(reinterpret_cast<GstAppSink*>(p_->sink), kPullTimeout);
    // A decoder that fell behind: the newest picture wins the slot.
    while (sample) {
        GstSample* next = a.app_sink_try_pull_sample(reinterpret_cast<GstAppSink*>(p_->sink), 0);
        if (!next) break;
        a.mini_object_unref(GST_MINI_OBJECT_CAST(sample));
        sample = next;
    }

    if (GstMessage* msg = a.bus_pop_filtered(p_->bus, GST_MESSAGE_ERROR)) {
        GError* err = nullptr;
        gchar* debug = nullptr;
        a.message_parse_error(msg, &err, &debug);
        MELLO_LOG_ERROR(TAG, "VA-API: GStreamer error: %s (%s)", err ? err->message : "?", debug ? debug : "");
        if (err) a.error_free(err);
        if (debug) a.free(debug);
        a.mini_object_unref(GST_MINI_OBJECT_CAST(msg));
        // A pipeline in error stops for good: the viewer takes the next decoder.
        failed_ = true;
        if (sample) a.mini_object_unref(GST_MINI_OBJECT_CAST(sample));
        return DecodeFeedResult::Error;
    }
    if (!sample) return DecodeFeedResult::Accepted;

    bool ok = false;
    GstBuffer* out = a.sample_get_buffer(sample);
    GstCaps* caps = a.sample_get_caps(sample);
    GstVideoInfo info;
    std::memset(&info, 0, sizeof(info));
    GstMapInfo map;
    std::memset(&map, 0, sizeof(map));
    if (out && caps && a.video_info_from_caps(&info, caps) && a.buffer_map(out, &map, GST_MAP_READ)) {
        size_t offset[2] = {GST_VIDEO_INFO_PLANE_OFFSET(&info, 0), GST_VIDEO_INFO_PLANE_OFFSET(&info, 1)};
        size_t stride[2] = {static_cast<size_t>(GST_VIDEO_INFO_PLANE_STRIDE(&info, 0)),
                            static_cast<size_t>(GST_VIDEO_INFO_PLANE_STRIDE(&info, 1))};
        // A buffer with its own layout carries it in the video meta.
        auto* meta = reinterpret_cast<GstVideoMeta*>(a.buffer_get_meta(out, a.video_meta_api_get_type()));
        if (meta && meta->n_planes >= 2) {
            offset[0] = meta->offset[0];
            offset[1] = meta->offset[1];
            stride[0] = static_cast<size_t>(meta->stride[0]);
            stride[1] = static_cast<size_t>(meta->stride[1]);
        }
        const uint32_t w = static_cast<uint32_t>(GST_VIDEO_INFO_WIDTH(&info)) & ~1u;
        const uint32_t h = static_cast<uint32_t>(GST_VIDEO_INFO_HEIGHT(&info)) & ~1u;
        const size_t need_y = offset[0] + stride[0] * (h - 1) + w;
        const size_t need_uv = offset[1] + stride[1] * (h / 2 - 1) + w;
        if (GST_VIDEO_INFO_FORMAT(&info) == GST_VIDEO_FORMAT_NV12 && w > 0 && h > 0 &&
            need_y <= map.size && need_uv <= map.size) {
            Slot& slot = slots_[slot_index];
            slot.nv12.resize(static_cast<size_t>(w) * h * 3 / 2);
            uint8_t* dst = slot.nv12.data();
            for (uint32_t row = 0; row < h; ++row) {
                std::memcpy(dst + static_cast<size_t>(row) * w, map.data + offset[0] + row * stride[0], w);
            }
            uint8_t* dst_uv = dst + static_cast<size_t>(w) * h;
            for (uint32_t row = 0; row < h / 2; ++row) {
                std::memcpy(dst_uv + static_cast<size_t>(row) * w, map.data + offset[1] + row * stride[1], w);
            }
            slot.width = w;
            slot.height = h;
            ok = true;
        } else {
            MELLO_LOG_ERROR(TAG, "VA-API: a picture that is not NV12 %ux%u in %zu bytes", w, h, map.size);
        }
        a.buffer_unmap(out, &map);
    }
    a.mini_object_unref(GST_MINI_OBJECT_CAST(sample));
    return ok ? DecodeFeedResult::FrameReady : DecodeFeedResult::Error;
}

bool GstVaDecoder::picture(int slot_index, CpuPicture& out) const {
    if (slot_index < 0 || slot_index >= kOutputSlots) return false;
    const Slot& slot = slots_[slot_index];
    if (slot.nv12.empty() || slot.width == 0 || slot.height == 0) return false;
    out.format = CpuPictureFormat::NV12;
    out.y = slot.nv12.data();
    out.u = slot.nv12.data() + static_cast<size_t>(slot.width) * slot.height;
    out.v = nullptr;
    out.y_stride = slot.width;
    out.uv_stride = slot.width;
    out.width = slot.width;
    out.height = slot.height;
    return true;
}

} // namespace mello::video
#endif
