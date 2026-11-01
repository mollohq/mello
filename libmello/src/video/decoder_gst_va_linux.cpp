#if defined(__linux__)
#include "decoder_gst_va.hpp"
#include "linux_gpu.hpp"
#include "../util/log.hpp"

#include <dlfcn.h>
#include <chrono>
#include <cstring>
#include <mutex>
#include <string>

// The real GStreamer headers give the types and the struct layouts. No
// function is linked: every call goes through a pointer from dlsym. The
// headers' inline helpers (gst_sample_unref, gst_buffer_unref, ...) call
// gst_mini_object_unref, so this file calls that directly. The GObject cast
// macros call g_type_check_instance_cast unless cast checks are off.
#define G_DISABLE_CAST_CHECKS 1
#include <gst/gst.h>
#include <gst/app/gstappsink.h>
#include <gst/app/gstappsrc.h>
#include <gst/video/video.h>

namespace mello::video {

static constexpr const char* TAG = "video/decoder";

// Waits this long for the picture of an access unit before decode() returns
// Accepted. A late picture comes out of the next call.
static constexpr GstClockTime kPullTimeout = 20 * GST_MSECOND;

namespace {

struct GstApi {
    bool loaded = false;
    decltype(&gst_init_check) init_check = nullptr;
    decltype(&gst_parse_launch) parse_launch = nullptr;
    decltype(&gst_bin_get_by_name) bin_get_by_name = nullptr;
    decltype(&gst_element_set_state) set_state = nullptr;
    decltype(&gst_element_get_state) get_state = nullptr;
    decltype(&gst_element_get_bus) get_bus = nullptr;
    decltype(&gst_element_factory_find) factory_find = nullptr;
    decltype(&gst_element_factory_make) factory_make = nullptr;
    decltype(&gst_object_unref) object_unref = nullptr;
    decltype(&gst_object_ref_sink) object_ref_sink = nullptr;
    decltype(&gst_mini_object_unref) mini_object_unref = nullptr;
    decltype(&gst_buffer_new_allocate) buffer_new_allocate = nullptr;
    decltype(&gst_buffer_fill) buffer_fill = nullptr;
    decltype(&gst_buffer_map) buffer_map = nullptr;
    decltype(&gst_buffer_unmap) buffer_unmap = nullptr;
    decltype(&gst_buffer_get_meta) buffer_get_meta = nullptr;
    decltype(&gst_sample_get_buffer) sample_get_buffer = nullptr;
    decltype(&gst_sample_get_caps) sample_get_caps = nullptr;
    decltype(&gst_bus_pop_filtered) bus_pop_filtered = nullptr;
    decltype(&gst_message_parse_error) message_parse_error = nullptr;
    decltype(&gst_app_src_push_buffer) app_src_push_buffer = nullptr;
    decltype(&gst_app_sink_try_pull_sample) app_sink_try_pull_sample = nullptr;
    decltype(&gst_video_info_from_caps) video_info_from_caps = nullptr;
    decltype(&gst_video_meta_api_get_type) video_meta_api_get_type = nullptr;
    decltype(&g_object_get) object_get = nullptr;
    decltype(&g_error_free) error_free = nullptr;
    decltype(&g_free) free = nullptr;
};

GstApi g_api;
std::once_flag g_load_once;

template<typename T>
bool sym(void* lib, const char* name, T& out) {
    out = reinterpret_cast<T>(dlsym(lib, name));
    if (!out) MELLO_LOG_WARN(TAG, "GStreamer: %s not found", name);
    return out != nullptr;
}

// Loads GStreamer once per process and keeps it loaded: GStreamer is not
// made to be unloaded.
void load_gstreamer() {
    void* gst = dlopen("libgstreamer-1.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    void* app = dlopen("libgstapp-1.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    void* video = dlopen("libgstvideo-1.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    void* gobject = dlopen("libgobject-2.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    void* glib = dlopen("libglib-2.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    if (!gst || !app || !video || !gobject || !glib) {
        MELLO_LOG_INFO(TAG, "GStreamer not found: no VA-API decoder");
        return;
    }
    GstApi& a = g_api;
    bool ok = sym(gst, "gst_init_check", a.init_check) && sym(gst, "gst_parse_launch", a.parse_launch) &&
              sym(gst, "gst_bin_get_by_name", a.bin_get_by_name) &&
              sym(gst, "gst_element_set_state", a.set_state) && sym(gst, "gst_element_get_state", a.get_state) &&
              sym(gst, "gst_element_get_bus", a.get_bus) &&
              sym(gst, "gst_element_factory_find", a.factory_find) &&
              sym(gst, "gst_element_factory_make", a.factory_make) &&
              sym(gst, "gst_object_unref", a.object_unref) && sym(gst, "gst_object_ref_sink", a.object_ref_sink) &&
              sym(gst, "gst_mini_object_unref", a.mini_object_unref) &&
              sym(gst, "gst_buffer_new_allocate", a.buffer_new_allocate) &&
              sym(gst, "gst_buffer_fill", a.buffer_fill) && sym(gst, "gst_buffer_map", a.buffer_map) &&
              sym(gst, "gst_buffer_unmap", a.buffer_unmap) && sym(gst, "gst_buffer_get_meta", a.buffer_get_meta) &&
              sym(gst, "gst_sample_get_buffer", a.sample_get_buffer) &&
              sym(gst, "gst_sample_get_caps", a.sample_get_caps) &&
              sym(gst, "gst_bus_pop_filtered", a.bus_pop_filtered) &&
              sym(gst, "gst_message_parse_error", a.message_parse_error) &&
              sym(app, "gst_app_src_push_buffer", a.app_src_push_buffer) &&
              sym(app, "gst_app_sink_try_pull_sample", a.app_sink_try_pull_sample) &&
              sym(video, "gst_video_info_from_caps", a.video_info_from_caps) &&
              sym(video, "gst_video_meta_api_get_type", a.video_meta_api_get_type) &&
              sym(gobject, "g_object_get", a.object_get) && sym(glib, "g_error_free", a.error_free) &&
              sym(glib, "g_free", a.free);
    if (!ok) return;
    GError* err = nullptr;
    if (!a.init_check(nullptr, nullptr, &err)) {
        MELLO_LOG_WARN(TAG, "GStreamer: gst_init failed: %s", err ? err->message : "?");
        if (err) a.error_free(err);
        return;
    }
    a.loaded = true;
}

bool gstreamer() {
    std::call_once(g_load_once, load_gstreamer);
    return g_api.loaded;
}

bool has_factory(const char* name) {
    GstElementFactory* f = g_api.factory_find(name);
    if (!f) return false;
    g_api.object_unref(f);
    return true;
}

// The device of a VA element, or empty.
std::string device_path_of(const char* factory) {
    GstElement* el = g_api.factory_make(factory, nullptr);
    if (!el) return {};
    g_api.object_ref_sink(el);
    gchar* path = nullptr;
    g_api.object_get(el, "device-path", &path, nullptr);
    std::string out = path ? path : "";
    if (path) g_api.free(path);
    g_api.object_unref(el);
    return out;
}

// The VA H.264 decoder element of the primary GPU. GStreamer registers
// `vah264dec` for the first VA device and `varenderD<N>h264dec` for others.
std::string find_va_element() {
    const std::string node = primary_render_node(list_linux_gpus());
    if (!node.empty()) {
        const std::string leaf = node.substr(node.rfind('/') + 1);  // renderD128
        const std::string named = "va" + leaf + "h264dec";  // varenderD129h264dec
        if (has_factory(named.c_str())) return named;
        if (has_factory("vah264dec") && device_path_of("vah264dec") == node) return "vah264dec";
    }
    return has_factory("vah264dec") ? "vah264dec" : std::string{};
}

// Opening a VA device to read its path costs a VA initialization, so the
// choice is made once per process. Empty without GStreamer or h264parse.
std::once_flag g_element_once;
std::string g_element;

const std::string& va_element_for_primary_gpu() {
    std::call_once(g_element_once, [] {
        if (gstreamer() && has_factory("h264parse")) g_element = find_va_element();
    });
    return g_element;
}

} // namespace

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
    return !va_element_for_primary_gpu().empty();
}

void GstVaDecoder::warm_up() {
    const auto t0 = std::chrono::steady_clock::now();
    const std::string& element = va_element_for_primary_gpu();
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
    if (!gstreamer()) return fail("GStreamer not found");
    if (!has_factory("h264parse")) return fail("no h264parse (gst-plugins-bad)");
    const std::string element = va_element_for_primary_gpu();
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
        GstElement* pipeline = g_api.parse_launch(desc->c_str(), &err);
        if (err) {
            MELLO_LOG_INFO(TAG, "VA-API: pipeline refused: %s", err->message);
            g_api.error_free(err);
            if (pipeline) g_api.object_unref(pipeline);
            continue;
        }
        p_->pipeline = pipeline;
        break;
    }
    if (!p_->pipeline) return fail("no pipeline");
    p_->src = g_api.bin_get_by_name(reinterpret_cast<GstBin*>(p_->pipeline), "src");
    p_->sink = g_api.bin_get_by_name(reinterpret_cast<GstBin*>(p_->pipeline), "sink");
    p_->bus = g_api.get_bus(p_->pipeline);
    if (!p_->src || !p_->sink || !p_->bus) return fail("pipeline without its source or sink");

    if (g_api.set_state(p_->pipeline, GST_STATE_PLAYING) == GST_STATE_CHANGE_FAILURE) {
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
        if (p_->pipeline) g_api.set_state(p_->pipeline, GST_STATE_NULL);
        if (p_->bus) g_api.object_unref(p_->bus);
        if (p_->src) g_api.object_unref(p_->src);
        if (p_->sink) g_api.object_unref(p_->sink);
        if (p_->pipeline) g_api.object_unref(p_->pipeline);
        p_.reset();
    }
    for (Slot& slot : slots_) slot = Slot{};
    frames_in_ = 0;
    failed_ = false;
}

DecodeFeedResult GstVaDecoder::decode(const uint8_t* data, size_t size, bool is_keyframe, int slot_index) {
    (void)is_keyframe;
    if (!p_ || failed_ || slot_index < 0 || slot_index >= kOutputSlots) return DecodeFeedResult::Error;
    const GstApi& a = g_api;

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
