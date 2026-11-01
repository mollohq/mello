#pragma once
#if defined(__linux__)
// GStreamer for the Linux VA-API decoder and encoder, loaded with dlopen.
//
// The real GStreamer headers give the types and the struct layouts. No
// function is linked: every call goes through a pointer from dlsym. The
// headers' inline helpers (gst_sample_unref, gst_buffer_unref, ...) call
// gst_mini_object_unref, so callers call that directly. The GObject cast
// macros call g_type_check_instance_cast unless cast checks are off.
#define G_DISABLE_CAST_CHECKS 1
#include <gst/gst.h>
#include <gst/app/gstappsink.h>
#include <gst/app/gstappsrc.h>
#include <gst/video/video.h>

#include <string>

namespace mello::video::gst {

struct Api {
    decltype(&gst_init_check) init_check = nullptr;
    decltype(&gst_parse_launch) parse_launch = nullptr;
    decltype(&gst_bin_get_by_name) bin_get_by_name = nullptr;
    decltype(&gst_element_set_state) set_state = nullptr;
    decltype(&gst_element_get_state) get_state = nullptr;
    decltype(&gst_element_get_bus) get_bus = nullptr;
    decltype(&gst_element_send_event) send_event = nullptr;
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
    decltype(&gst_caps_from_string) caps_from_string = nullptr;
    decltype(&gst_sample_get_buffer) sample_get_buffer = nullptr;
    decltype(&gst_sample_get_caps) sample_get_caps = nullptr;
    decltype(&gst_bus_pop_filtered) bus_pop_filtered = nullptr;
    decltype(&gst_message_parse_error) message_parse_error = nullptr;
    decltype(&gst_app_src_push_buffer) app_src_push_buffer = nullptr;
    decltype(&gst_app_sink_try_pull_sample) app_sink_try_pull_sample = nullptr;
    decltype(&gst_video_info_from_caps) video_info_from_caps = nullptr;
    decltype(&gst_video_meta_api_get_type) video_meta_api_get_type = nullptr;
    decltype(&gst_video_event_new_downstream_force_key_unit) force_key_unit_event = nullptr;
    decltype(&g_object_get) object_get = nullptr;
    decltype(&g_object_set) object_set = nullptr;
    decltype(&g_error_free) error_free = nullptr;
    decltype(&g_free) free = nullptr;
};

/// Loads GStreamer once per process and keeps it loaded. True when every
/// entry point resolved and gst_init succeeded. Any thread.
bool load();

/// The entry points. Valid after load() returned true.
const Api& api();

/// True when GStreamer has an element factory of that name.
bool has_factory(const char* name);

/// The VA element of `kind` ("h264dec", "h264enc", "postproc") on the
/// primary GPU: `va<kind>` on GStreamer's first VA device, else
/// `varenderD<N><kind>`. Empty when GStreamer or the element is missing.
/// Chosen once per process and kind: reading an element's device opens it.
std::string va_element(const std::string& kind);

/// Logs and clears the first error on `bus`. True when there was one.
bool pop_bus_error(GstBus* bus, const char* who);

} // namespace mello::video::gst
#endif
