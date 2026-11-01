#if defined(__linux__)
#include "gst_runtime_linux.hpp"
#include "linux_gpu.hpp"
#include "../util/log.hpp"

#include <dlfcn.h>
#include <map>
#include <mutex>

namespace mello::video::gst {

static constexpr const char* TAG = "video/gstreamer";

namespace {

Api g_api;
bool g_loaded = false;
bool g_dmabuf = false;
std::once_flag g_load_once;

template<typename T>
bool sym(void* lib, const char* name, T& out) {
    out = reinterpret_cast<T>(dlsym(lib, name));
    if (!out) MELLO_LOG_WARN(TAG, "GStreamer: %s not found", name);
    return out != nullptr;
}

// Loads GStreamer once per process and keeps it loaded: GStreamer is not
// made to be unloaded.
void load_once() {
    // Normally done by mello_init already (warm_linux_decoders); tests and
    // tools load GStreamer without it.
    prepare_va_driver_env();
    void* gst = dlopen("libgstreamer-1.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    void* app = dlopen("libgstapp-1.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    void* video = dlopen("libgstvideo-1.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    void* gobject = dlopen("libgobject-2.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    void* glib = dlopen("libglib-2.0.so.0", RTLD_NOW | RTLD_GLOBAL);
    if (!gst || !app || !video || !gobject || !glib) {
        MELLO_LOG_INFO(TAG, "GStreamer not found: no VA-API decoder or encoder");
        return;
    }
    Api& a = g_api;
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
              sym(gst, "gst_element_send_event", a.send_event) &&
              sym(gst, "gst_caps_from_string", a.caps_from_string) &&
              sym(video, "gst_video_event_new_downstream_force_key_unit", a.force_key_unit_event) &&
              sym(gobject, "g_object_get", a.object_get) && sym(gobject, "g_object_set", a.object_set) &&
              sym(glib, "g_error_free", a.error_free) &&
              sym(glib, "g_free", a.free);
    ok = ok && sym(gst, "gst_buffer_new", a.buffer_new) &&
         sym(gst, "gst_buffer_append_memory", a.buffer_append_memory) &&
         sym(gst, "gst_buffer_add_meta", a.buffer_add_meta) &&
         sym(gst, "gst_mini_object_weak_ref", a.mini_object_weak_ref) &&
         sym(gst, "gst_element_get_static_pad", a.element_get_static_pad) &&
         sym(gst, "gst_pad_query_caps", a.pad_query_caps) && sym(gst, "gst_caps_to_string", a.caps_to_string) &&
         sym(video, "gst_buffer_add_video_meta_full", a.buffer_add_video_meta_full) &&
         sym(video, "gst_video_crop_meta_get_info", a.video_crop_meta_get_info) &&
         sym(video, "gst_video_format_from_string", a.video_format_from_string);
    if (!ok) return;
    // DMA-BUF import is optional: without it the encoder takes CPU pictures.
    if (void* alloc = dlopen("libgstallocators-1.0.so.0", RTLD_NOW | RTLD_GLOBAL)) {
        g_dmabuf = sym(alloc, "gst_dmabuf_allocator_new", a.dmabuf_allocator_new) &&
                   sym(alloc, "gst_dmabuf_allocator_alloc", a.dmabuf_allocator_alloc);
    }
    GError* err = nullptr;
    if (!a.init_check(nullptr, nullptr, &err)) {
        MELLO_LOG_WARN(TAG, "GStreamer: gst_init failed: %s", err ? err->message : "?");
        if (err) a.error_free(err);
        return;
    }
    g_loaded = true;
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

std::string find_va_element(const std::string& kind) {
    const std::string first = "va" + kind;  // vah264enc
    const std::string node = primary_render_node(list_linux_gpus());
    if (!node.empty()) {
        const std::string leaf = node.substr(node.rfind('/') + 1);  // renderD128
        const std::string named = "va" + leaf + kind;               // varenderD129h264enc
        if (has_factory(named.c_str())) return named;
        if (has_factory(first.c_str()) && device_path_of(first.c_str()) == node) return first;
    }
    return has_factory(first.c_str()) ? first : std::string{};
}

} // namespace

bool load() {
    std::call_once(g_load_once, load_once);
    return g_loaded;
}

const Api& api() { return g_api; }

bool has_dmabuf() { return load() && g_dmabuf; }

bool has_factory(const char* name) {
    if (!load()) return false;
    GstElementFactory* f = g_api.factory_find(name);
    if (!f) return false;
    g_api.object_unref(f);
    return true;
}

std::string va_element(const std::string& kind) {
    static std::mutex mutex;
    static std::map<std::string, std::string> chosen;
    std::lock_guard<std::mutex> lock(mutex);
    auto it = chosen.find(kind);
    if (it != chosen.end()) return it->second;
    std::string element = load() ? find_va_element(kind) : std::string{};
    chosen.emplace(kind, element);
    return element;
}

bool pop_bus_error(GstBus* bus, const char* who) {
    GstMessage* msg = bus ? g_api.bus_pop_filtered(bus, GST_MESSAGE_ERROR) : nullptr;
    if (!msg) return false;
    GError* err = nullptr;
    gchar* debug = nullptr;
    g_api.message_parse_error(msg, &err, &debug);
    MELLO_LOG_ERROR(TAG, "%s: GStreamer error: %s (%s)", who, err ? err->message : "?", debug ? debug : "");
    if (err) g_api.error_free(err);
    if (debug) g_api.free(debug);
    g_api.mini_object_unref(GST_MINI_OBJECT_CAST(msg));
    return true;
}

} // namespace mello::video::gst
#endif
