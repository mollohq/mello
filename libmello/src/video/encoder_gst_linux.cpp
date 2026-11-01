#if defined(__linux__)
#include "encoder_gst.hpp"
#include "gst_runtime_linux.hpp"
#include "linux_gpu.hpp"
#include "yuv_convert.hpp"
#include "../util/log.hpp"

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <unistd.h>

namespace mello::video {

static constexpr const char* TAG = "video/encoder";

// How long encode() waits for the access unit of the picture it pushed. A
// later unit comes out of the next call; none is dropped.
static constexpr GstClockTime kPullTimeout = 50 * GST_MSECOND;

std::vector<DmaBufFormat> parse_rgb_drm_formats(const std::string& caps) {
    static const struct { const char* name; uint32_t fourcc; } kRgb[] = {
        {"XR24", kDrmXRGB8888}, {"AR24", kDrmARGB8888}, {"XB24", kDrmXBGR8888}, {"AB24", kDrmABGR8888}};
    std::vector<DmaBufFormat> out;
    auto add = [&out](uint32_t fourcc, uint64_t modifier) {
        for (DmaBufFormat& f : out) {
            if (f.drm_fourcc == fourcc) {
                if (std::find(f.modifiers.begin(), f.modifiers.end(), modifier) == f.modifiers.end()) {
                    f.modifiers.push_back(modifier);
                }
                return;
            }
        }
        out.push_back(DmaBufFormat{fourcc, {modifier}});
    };
    // Tokens are separated by everything a caps string puts between values.
    std::string token;
    auto flush = [&]() {
        for (const auto& rgb : kRgb) {
            const size_t n = std::strlen(rgb.name);
            if (token.compare(0, n, rgb.name) != 0) continue;
            if (token.size() == n) {
                add(rgb.fourcc, 0);
            } else if (token.compare(n, 3, ":0x") == 0) {
                add(rgb.fourcc, std::strtoull(token.c_str() + n + 3, nullptr, 16));
            }
        }
        token.clear();
    };
    for (char c : caps) {
        if (c == ' ' || c == ',' || c == '{' || c == '}' || c == '"' || c == '=' || c == ';' || c == '(' ||
            c == ')' || c == '\\') {
            flush();
        } else {
            token.push_back(c);
        }
    }
    flush();
    return out;
}

// The RGB DMA-BUF layouts in the DMABuf part of a pad's caps.
static std::vector<DmaBufFormat> pad_dmabuf_formats(GstElement* el, const char* pad_name) {
    const gst::Api& a = gst::api();
    std::vector<DmaBufFormat> formats;
    if (GstPad* pad = a.element_get_static_pad(el, pad_name)) {
        if (GstCaps* caps = a.pad_query_caps(pad, nullptr)) {
            if (gchar* text = a.caps_to_string(caps)) {
                // Only the DMABuf part: other parts list the same names as
                // plain formats.
                std::string all(text);
                a.free(text);
                const size_t at = all.find("memory:DMABuf");
                if (at != std::string::npos) formats = parse_rgb_drm_formats(all.substr(at));
            }
            a.mini_object_unref(GST_MINI_OBJECT_CAST(caps));
        }
        a.object_unref(pad);
    }
    return formats;
}

static std::vector<DmaBufFormat> va_dmabuf_formats() {
    const gst::Api& a = gst::api();
    const std::string post = gst::va_element("postproc");
    if (post.empty()) return {};
    GstElement* el = a.factory_make(post.c_str(), nullptr);
    if (!el) return {};
    a.object_ref_sink(el);
    auto formats = pad_dmabuf_formats(el, "sink");
    a.object_unref(el);
    return formats;
}

// glupload lists the modifiers of its GL context once it has one: a short
// running pipeline gives it one. Best effort; empty means shared memory.
static std::vector<DmaBufFormat> gl_dmabuf_formats() {
    const gst::Api& a = gst::api();
    GError* err = nullptr;
    GstElement* pipeline = a.parse_launch(
        "appsrc name=src is-live=true format=time caps=video/x-raw,format=BGRx,width=64,height=64,framerate=60/1 "
        "! glupload name=up ! fakesink",
        &err);
    if (err) {
        a.error_free(err);
        if (pipeline) a.object_unref(pipeline);
        return {};
    }
    std::vector<DmaBufFormat> formats;
    if (a.set_state(pipeline, GST_STATE_PLAYING) != GST_STATE_CHANGE_FAILURE) {
        a.get_state(pipeline, nullptr, nullptr, 500 * GST_MSECOND);
        if (GstElement* up = a.bin_get_by_name(reinterpret_cast<GstBin*>(pipeline), "up")) {
            formats = pad_dmabuf_formats(up, "sink");
            a.object_unref(up);
        }
    }
    a.set_state(pipeline, GST_STATE_NULL);
    a.object_unref(pipeline);
    return formats;
}

bool GstEncoder::nvenc_gpu_path() {
    if (const char* forced = std::getenv("MELLO_NVENC_GL")) return std::strcmp(forced, "1") == 0;
    for (const LinuxGpu& g : list_linux_gpus()) {
        if (g.primary) return g.vendor == kVendorNvidia;
    }
    const auto gpus = list_linux_gpus();
    return gpus.size() == 1 && gpus[0].vendor == kVendorNvidia;
}

std::vector<DmaBufFormat> GstEncoder::dmabuf_formats(GstEncoderKind kind) {
    static std::once_flag once[2];
    static std::vector<DmaBufFormat> formats[2];
    const int i = kind == GstEncoderKind::VaApi ? 0 : 1;
    std::call_once(once[i], [kind, i] {
        if (!gst::has_dmabuf()) return;
        const gst::Api& a = gst::api();
        if (a.video_format_from_string("DMA_DRM") == GST_VIDEO_FORMAT_UNKNOWN) {
            MELLO_LOG_INFO(TAG, "GStreamer older than 1.24: no DMA-BUF import");
            return;
        }
        if (kind == GstEncoderKind::VaApi) {
            formats[i] = va_dmabuf_formats();
        } else if (nvenc_gpu_path()) {
            formats[i] = gl_dmabuf_formats();
        }
        for (const DmaBufFormat& f : formats[i]) {
            MELLO_LOG_INFO(TAG, "%s encoder imports DMA-BUF %.4s with %zu modifier(s)",
                           kind == GstEncoderKind::VaApi ? "VA-API" : "NVENC",
                           reinterpret_cast<const char*>(&f.drm_fourcc), f.modifiers.size());
        }
        if (formats[i].empty()) {
            MELLO_LOG_INFO(TAG, "%s encoder: no DMA-BUF import, the capture sends shared memory",
                           kind == GstEncoderKind::VaApi ? "VA-API" : "NVENC");
        }
    });
    return formats[i];
}

static std::string dmabuf_caps(const CaptureFrame& f, uint32_t fps) {
    char drm[48];
    const char* cc = reinterpret_cast<const char*>(&f.drm_fourcc);
    if (f.modifier == 0) {
        std::snprintf(drm, sizeof(drm), "%.4s", cc);
    } else {
        std::snprintf(drm, sizeof(drm), "%.4s:0x%016llx", cc, static_cast<unsigned long long>(f.modifier));
    }
    return "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=" + std::string(drm) +
           ",width=" + std::to_string(f.buffer_width) + ",height=" + std::to_string(f.buffer_height) +
           ",framerate=" + std::to_string(fps > 0 ? fps : 60) + "/1";
}

// The GstBuffer of a DMA-BUF frame keeps the frame's release token until
// GStreamer frees it: then the capture gets its PipeWire buffer back.
static void drop_release(gpointer data, GstMiniObject* /*buffer*/) {
    delete static_cast<std::shared_ptr<FrameRelease>*>(data);
}

struct GstEncoder::Pipeline {
    GstElement* pipeline = nullptr;
    GstElement* src = nullptr;
    GstElement* enc = nullptr;
    GstElement* sink = nullptr;
    GstBus* bus = nullptr;
};

GstEncoder::GstEncoder(GstEncoderKind kind) : kind_(kind) {}

GstEncoder::~GstEncoder() {
    shutdown();
}

bool GstEncoder::is_available(GstEncoderKind kind) {
    if (kind == GstEncoderKind::VaApi) {
        return !gst::va_element("h264enc").empty() && !gst::va_element("postproc").empty();
    }
    if (!gst::has_factory("nvh264enc")) return false;
    return !nvenc_gpu_path() ||
           (gst::has_factory("glupload") && gst::has_factory("glcolorscale") && gst::has_factory("glcolorconvert"));
}

static std::string nv12_caps(uint32_t w, uint32_t h, uint32_t fps) {
    return "video/x-raw,format=NV12,width=" + std::to_string(w) + ",height=" + std::to_string(h) +
           ",framerate=" + std::to_string(fps > 0 ? fps : 60) + "/1,colorimetry=bt709";
}

// The frame rate matters: VA rate control spends bitrate / framerate per
// picture. Without it the encoder assumed 30 fps and a 49 fps stream ran
// at 8 Mbps for a 5 Mbps target (2026-11-01).
static std::string src_caps(uint32_t w, uint32_t h, uint32_t fps) {
    return "video/x-raw,format=BGRx,width=" + std::to_string(w) + ",height=" + std::to_string(h) +
           ",framerate=" + std::to_string(fps > 0 ? fps : 60) + "/1";
}

bool GstEncoder::initialize(const GraphicsDevice& device, const EncoderConfig& config) {
    (void)device;
    auto fail = [this](const char* why) {
        MELLO_LOG_INFO(TAG, "%s encoder not used: %s", name(), why);
        shutdown();
        return false;
    };
    if (config.codec != VideoCodec::H264) return fail("not H.264");
    if (config.width == 0 || config.height == 0 || (config.width & 1) || (config.height & 1)) {
        return fail("invalid size");
    }
    if (!is_available(kind_)) {
        return fail(kind_ == GstEncoderKind::VaApi ? "no VA H.264 encoder or post-processor (gst-plugin-va)"
                                                   : "no nvh264enc (gst-plugins-bad nvcodec) or GL elements");
    }
    const bool va = kind_ == GstEncoderKind::VaApi;
    const std::string enc = va ? gst::va_element("h264enc") : "nvh264enc";
    const std::string post = va ? gst::va_element("postproc") : "";
    cpu_convert_ = !va && !nvenc_gpu_path();
    config_ = config;

    const std::string w = std::to_string(config.width), h = std::to_string(config.height);
    const std::string kbps = std::to_string(config.bitrate_kbps);
    const std::string gop = std::to_string(config.keyframe_interval);
    fps_ = config.fps > 0 ? config.fps : 60;
    const std::string first_caps =
        cpu_convert_ ? nv12_caps(config.width, config.height, fps_) : src_caps(config.width, config.height, fps_);
    // NVENC's GL chain does not renegotiate a new input size (glupload finds
    // no output for it), so its appsrc gets its caps with the first picture
    // and a later change rebuilds the pipeline (encode()).
    const bool gl_chain = !va && !cpu_convert_;
    const std::string head = std::string("appsrc name=src is-live=true format=time do-timestamp=false") +
                             (gl_chain ? "" : " caps=" + first_caps);
    const std::string tail = " ! video/x-h264,profile=high,stream-format=byte-stream,alignment=au"
                             " ! appsink name=sink sync=false max-buffers=16 drop=false";
    // Full settings first. A GStreamer without one of the properties, or a
    // driver that refuses BT.709 in the caps, gets the next, simpler line.
    std::string full, plain;
    if (va) {
        full = head + " ! " + post + " ! video/x-raw(memory:VAMemory),format=NV12,width=" + w + ",height=" + h +
               ",colorimetry=bt709 ! " + enc + " name=enc rate-control=vbr bitrate=" + kbps +
               " target-percentage=80 key-int-max=" + gop + " b-frames=0 ref-frames=1 target-usage=7" + tail;
        plain = head + " ! " + post + " ! video/x-raw(memory:VAMemory),format=NV12,width=" + w + ",height=" + h +
                " ! " + enc + " name=enc bitrate=" + kbps + " key-int-max=" + gop + " b-frames=0" + tail;
    } else {
        // The Windows NVENC settings (encoder_nvenc.cpp): P4 with
        // ultra-low-latency tuning, VBR up to 1.25x, a VBV of half a second
        // of the peak (at least four frames), AQ, SPS/PPS on every IDR.
        const uint32_t max_kbps = config.bitrate_kbps + config.bitrate_kbps / 4;
        const uint32_t vbv_kbits = std::max(max_kbps / 2, config.bitrate_kbps * 4 / std::max<uint32_t>(fps_, 1));
        const std::string nv = " ! nvh264enc name=enc preset=p4 tune=ultra-low-latency rc-mode=vbr bitrate=" + kbps +
                               " max-bitrate=" + std::to_string(max_kbps) +
                               " vbv-buffer-size=" + std::to_string(vbv_kbits) + " gop-size=" + gop +
                               " bframes=0 rc-lookahead=0 zerolatency=true spatial-aq=true temporal-aq=true"
                               " aq-strength=8 repeat-sequence-header=true";
        const std::string nv_plain = " ! nvh264enc name=enc bitrate=" + kbps + " gop-size=" + gop + " bframes=0";
        // GPU path: GL imports, converts to RGBA (glcolorscale scales RGBA
        // only), scales, and converts to NV12 (BT.709). Each step checked
        // with gst-launch on the laptop's NVIDIA GPU (2026-11-01). CPU path:
        // the caps are NV12 at the encode size already.
        const std::string gl = " ! glupload ! glcolorconvert ! video/x-raw(memory:GLMemory),format=RGBA ! "
                               "glcolorscale ! video/x-raw(memory:GLMemory),format=RGBA,width=" + w +
                               ",height=" + h + " ! glcolorconvert ! video/x-raw(memory:GLMemory),format=NV12";
        full = head + (cpu_convert_ ? "" : gl + ",colorimetry=bt709") + nv + tail;
        plain = head + (cpu_convert_ ? "" : gl) + nv_plain + tail;
    }

    const gst::Api& a = gst::api();
    p_ = std::make_unique<Pipeline>();
    for (const std::string* desc : {&full, &plain}) {
        GError* err = nullptr;
        GstElement* pipeline = a.parse_launch(desc->c_str(), &err);
        if (err) {
            MELLO_LOG_INFO(TAG, "%s encoder: pipeline refused: %s", name(), err->message);
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
        MELLO_LOG_INFO(TAG, "%s encoder: pipeline does not start, trying the simpler one", name());
        shutdown();
        p_ = std::make_unique<Pipeline>();
    }
    if (!p_->pipeline) return fail("no pipeline");
    input_caps_ = gl_chain ? std::string{} : first_caps;
    if (gst::has_dmabuf() && !dmabuf_allocator_) {
        dmabuf_allocator_ = a.dmabuf_allocator_new();
    }
    stats_ = {};
    stats_window_start_ = {};
    backlog_.clear();
    first_pts_us_ = 0;
    in_flight_ = 0;
    failed_ = false;
    MELLO_LOG_INFO(TAG, "Selected encoder: %s (GStreamer %s%s%s) %ux%u %u fps %u kbps idr_interval=%u", name(),
                   enc.c_str(), va ? ", " : "", va ? post.c_str() : (cpu_convert_ ? ", CPU convert" : ", GL"),
                   config.width, config.height, config.fps, config.bitrate_kbps, config.keyframe_interval);
    return true;
}

void GstEncoder::shutdown() {
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
    if (dmabuf_allocator_) {
        a.object_unref(dmabuf_allocator_);
        dmabuf_allocator_ = nullptr;
    }
}

bool GstEncoder::encode(const CaptureFrame& frame, uint64_t capture_us, EncodedPacket& out) {
    if (!p_ || failed_ || (!frame.data && !frame.is_dmabuf()) || frame.width < 2 || frame.height < 2) {
        return false;
    }
    const gst::Api& a = gst::api();
    const auto t0 = std::chrono::steady_clock::now();

    // Controls from other threads, before this picture.
    const uint32_t kbps = pending_bitrate_kbps_.exchange(0, std::memory_order_relaxed);
    if (kbps > 0 && kbps != config_.bitrate_kbps) {
        a.object_set(p_->enc, "bitrate", static_cast<guint>(kbps), nullptr);
        if (kind_ == GstEncoderKind::Nvenc) {
            a.object_set(p_->enc, "max-bitrate", static_cast<guint>(kbps + kbps / 4), nullptr);
        }
        config_.bitrate_kbps = kbps;
    }
    if (force_idr_.exchange(false, std::memory_order_relaxed)) {
        GstEvent* ev = a.force_key_unit_event(GST_CLOCK_TIME_NONE, GST_CLOCK_TIME_NONE, GST_CLOCK_TIME_NONE,
                                              TRUE, 0);
        a.send_event(p_->enc, ev);
    }

    const uint32_t fps = pending_fps_.exchange(0, std::memory_order_relaxed);
    const bool new_fps = fps > 0 && fps != fps_;
    if (new_fps) fps_ = fps;

    // The input caps follow the frame: CPU or DMA-BUF, its size (a resized
    // window), its layout, and the frame rate. vapostproc imports, converts
    // and scales on the GPU; the encoder takes the rate for its budget.
    if (cpu_convert_ && frame.is_dmabuf()) return false;  // the capture offers no DMA-BUF then
    const std::string caps_text = cpu_convert_        ? nv12_caps(config_.width, config_.height, fps_)
                                  : frame.is_dmabuf() ? dmabuf_caps(frame, fps_)
                                                      : src_caps(frame.width, frame.height, fps_);
    if (kind_ == GstEncoderKind::Nvenc && !cpu_convert_ && !input_caps_.empty() && caps_text != input_caps_) {
        // A new layout for the GL chain: a new pipeline, which starts with an
        // IDR. The access units still in flight go with the old one.
        MELLO_LOG_INFO(TAG, "NVENC encoder: new input, rebuilding the pipeline");
        const EncoderConfig cfg = config_;
        const uint32_t fps_now = fps_;
        shutdown();
        if (!initialize(GraphicsDevice{}, cfg)) {
            failed_ = true;
            return false;
        }
        fps_ = fps_now;
    }
    if (caps_text != input_caps_ || new_fps) {
        GstCaps* caps = a.caps_from_string(caps_text.c_str());
        if (!caps) return false;
        a.object_set(p_->src, "caps", caps, nullptr);
        a.mini_object_unref(GST_MINI_OBJECT_CAST(caps));
        input_caps_ = caps_text;
        MELLO_LOG_INFO(TAG, "VA-API encoder input: %s", caps_text.c_str());
    }

    GstBuffer* buf = nullptr;
    if (cpu_convert_) {
        nv12_.resize(static_cast<size_t>(config_.width) * config_.height * 3 / 2);
        rgbx_to_nv12(frame, config_.width, config_.height, nv12_.data());
        buf = a.buffer_new_allocate(nullptr, nv12_.size(), nullptr);
        if (!buf) return false;
        a.buffer_fill(buf, 0, nv12_.data(), nv12_.size());
    } else if (frame.is_dmabuf()) {
        if (!dmabuf_allocator_) return false;
        // GStreamer closes its own duplicate; the capture's descriptor stays.
        const int fd = dup(frame.dmabuf_fd);
        if (fd < 0) return false;
        GstMemory* mem = a.dmabuf_allocator_alloc(static_cast<GstAllocator*>(dmabuf_allocator_), fd,
                                                  frame.dmabuf_size);
        if (!mem) {
            close(fd);
            return false;
        }
        buf = a.buffer_new();
        a.buffer_append_memory(buf, mem);
        gsize offset[GST_VIDEO_MAX_PLANES] = {frame.dmabuf_offset};
        gint stride[GST_VIDEO_MAX_PLANES] = {static_cast<gint>(frame.stride)};
        a.buffer_add_video_meta_full(buf, GST_VIDEO_FRAME_FLAG_NONE, a.video_format_from_string("DMA_DRM"),
                                     frame.buffer_width, frame.buffer_height, 1, offset, stride);
        if (frame.crop_x != 0 || frame.crop_y != 0 || frame.width != frame.buffer_width ||
            frame.height != frame.buffer_height) {
            auto* crop = reinterpret_cast<GstVideoCropMeta*>(
                a.buffer_add_meta(buf, a.video_crop_meta_get_info(), nullptr));
            if (crop) {
                crop->x = frame.crop_x;
                crop->y = frame.crop_y;
                crop->width = frame.width;
                crop->height = frame.height;
            }
        }
        if (frame.release) {
            a.mini_object_weak_ref(GST_MINI_OBJECT_CAST(buf), &drop_release,
                                   new std::shared_ptr<FrameRelease>(frame.release));
        }
    } else {
        const size_t row = static_cast<size_t>(frame.width) * 4;
        buf = a.buffer_new_allocate(nullptr, row * frame.height, nullptr);
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

    ++in_flight_;
    const auto t2 = std::chrono::steady_clock::now();
    phases_.lock_ms = std::chrono::duration<double, std::milli>(t1 - t0).count();
    phases_.submit_ms = std::chrono::duration<double, std::milli>(t2 - t1).count();
    // One picture stays in flight: wait only for the earlier one's unit.
    const uint64_t wait = in_flight_ > 1 ? kPullTimeout : 0;
    const bool got = collect(wait, capture_us, out);
    phases_.wait_ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t2).count();
    return got;
}

bool GstEncoder::drain(EncodedPacket& out) {
    if (!p_ || failed_) return false;
    return collect(0, 0, out);
}

bool GstEncoder::collect(uint64_t timeout_ns, uint64_t capture_us, EncodedPacket& out) {
    const gst::Api& a = gst::api();
    // Every access unit that is ready, in order.
    GstClockTime wait = timeout_ns;
    while (GstSample* sample = a.app_sink_try_pull_sample(reinterpret_cast<GstAppSink*>(p_->sink), wait)) {
        wait = 0;
        if (in_flight_ > 0) --in_flight_;
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
    if (gst::pop_bus_error(p_->bus, name())) {
        // A pipeline in error stops for good.
        failed_ = true;
        return false;
    }
    if (backlog_.empty()) return false;

    out = std::move(backlog_.front());
    backlog_.pop_front();
    const auto now = std::chrono::steady_clock::now();
    stats_.bytes_sent += out.data.size();
    if (out.is_keyframe) stats_.keyframes_sent++;
    stats_window_frames_++;
    stats_window_bytes_ += out.data.size();
    if (stats_window_start_.time_since_epoch().count() == 0) stats_window_start_ = now;
    const auto elapsed_us = std::chrono::duration_cast<std::chrono::microseconds>(now - stats_window_start_).count();
    if (elapsed_us >= 1'000'000) {
        stats_.fps_actual = static_cast<uint32_t>((static_cast<uint64_t>(stats_window_frames_) * 1'000'000) /
                                                  static_cast<uint64_t>(elapsed_us));
        stats_.bitrate_kbps = static_cast<uint32_t>((stats_window_bytes_ * 8) / static_cast<uint64_t>(elapsed_us / 1'000));
        stats_window_frames_ = 0;
        stats_window_bytes_ = 0;
        stats_window_start_ = now;
    }
    return true;
}

void GstEncoder::set_framerate(uint32_t fps) {
    if (fps > 0) pending_fps_.store(fps, std::memory_order_relaxed);
}

void GstEncoder::request_keyframe() {
    force_idr_.store(true, std::memory_order_relaxed);
}

void GstEncoder::set_bitrate(uint32_t kbps) {
    if (kbps > 0) pending_bitrate_kbps_.store(kbps, std::memory_order_relaxed);
}

void GstEncoder::get_stats(EncoderStats& out) const {
    out = stats_;
}

void GstEncoder::get_phase_timing(EncodePhaseTiming& out) const {
    out = phases_;
}

} // namespace mello::video
#endif
