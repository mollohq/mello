#if defined(__linux__)
#include "capture_pipewire_video.hpp"
#include "cpu_frame.hpp"
#include "../audio/pipewire_stream.hpp"
#include "../util/log.hpp"

#include <pipewire/pipewire.h>
#include <spa/buffer/meta.h>
#include <spa/param/video/format-utils.h>
#include <spa/pod/builder.h>

#include <atomic>
#include <chrono>
#include <mutex>
#include <unistd.h>

namespace mello::video {

static constexpr const char* TAG = "video/capture";

// How long initialize() waits for the stream's format. The portal's node
// exists already, so this is the negotiation alone.
static constexpr auto kFormatWait = std::chrono::seconds(3);

static uint64_t steady_us() {
    return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::microseconds>(
                                     std::chrono::steady_clock::now().time_since_epoch())
                                     .count());
}

struct PipeWireVideoCapture::Impl {
    pw_thread_loop* loop = nullptr;
    pw_context* context = nullptr;
    pw_core* core = nullptr;
    pw_stream* stream = nullptr;
    spa_hook listener{};

    std::atomic<uint32_t> width{0};
    std::atomic<uint32_t> height{0};
    CpuPixelFormat format = CpuPixelFormat::BGRx;  // loop thread
    bool format_ready = false;                      // loop lock
    std::atomic<bool> failed{false};
    std::atomic<bool> streamed{false};

    std::mutex callback_mutex;
    FrameCallback callback;
    uint64_t frames = 0;  // loop thread

    static void on_state_changed(void* data, pw_stream_state old, pw_stream_state state, const char* error);
    static void on_param_changed(void* data, uint32_t id, const spa_pod* param);
    static void on_process(void* data);
    void teardown();
};

static const pw_stream_events kStreamEvents = [] {
    pw_stream_events e{};
    e.version = PW_VERSION_STREAM_EVENTS;
    e.state_changed = &PipeWireVideoCapture::Impl::on_state_changed;
    e.param_changed = &PipeWireVideoCapture::Impl::on_param_changed;
    e.process = &PipeWireVideoCapture::Impl::on_process;
    return e;
}();

void PipeWireVideoCapture::Impl::on_state_changed(void* data, pw_stream_state old, pw_stream_state state,
                                                  const char* error) {
    auto* self = static_cast<Impl*>(data);
    MELLO_LOG_INFO(TAG, "PipeWire screencast: %s -> %s%s%s", pw_stream_state_as_string(old),
                   pw_stream_state_as_string(state), error ? ": " : "", error ? error : "");
    if (state == PW_STREAM_STATE_STREAMING) self->streamed.store(true);
    // The portal session ended (the user stopped sharing in the desktop), or
    // PipeWire gave up: the stream does not come back.
    if (state == PW_STREAM_STATE_ERROR ||
        (state == PW_STREAM_STATE_UNCONNECTED && self->streamed.load())) {
        self->failed.store(true);
    }
    pw_thread_loop_signal(self->loop, false);
}

void PipeWireVideoCapture::Impl::on_param_changed(void* data, uint32_t id, const spa_pod* param) {
    auto* self = static_cast<Impl*>(data);
    if (id != SPA_PARAM_Format || !param) return;

    uint32_t media_type = 0, media_subtype = 0;
    if (spa_format_parse(param, &media_type, &media_subtype) < 0 || media_type != SPA_MEDIA_TYPE_video ||
        media_subtype != SPA_MEDIA_SUBTYPE_raw) {
        return;
    }
    spa_video_info_raw info{};
    if (spa_format_video_raw_parse(param, &info) < 0) return;
    switch (info.format) {
    case SPA_VIDEO_FORMAT_BGRx:
    case SPA_VIDEO_FORMAT_BGRA:
        self->format = CpuPixelFormat::BGRx;
        break;
    case SPA_VIDEO_FORMAT_RGBx:
    case SPA_VIDEO_FORMAT_RGBA:
        self->format = CpuPixelFormat::RGBx;
        break;
    default:
        MELLO_LOG_ERROR(TAG, "PipeWire screencast: unexpected format %u", info.format);
        return;
    }
    self->width.store(info.size.width);
    self->height.store(info.size.height);
    MELLO_LOG_INFO(TAG, "PipeWire screencast: %ux%u %s, %u/%u fps", info.size.width, info.size.height,
                   self->format == CpuPixelFormat::BGRx ? "BGRx" : "RGBx", info.max_framerate.num,
                   info.max_framerate.denom);

    // Shared memory buffers, and the crop region a window capture may carry.
    uint8_t buffer[512];
    spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
    const spa_pod* params[2];
    params[0] = static_cast<const spa_pod*>(spa_pod_builder_add_object(
        &b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers, SPA_PARAM_BUFFERS_dataType,
        SPA_POD_CHOICE_FLAGS_Int((1 << SPA_DATA_MemPtr) | (1 << SPA_DATA_MemFd))));
    params[1] = static_cast<const spa_pod*>(spa_pod_builder_add_object(
        &b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta, SPA_PARAM_META_type, SPA_POD_Id(SPA_META_VideoCrop),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(spa_meta_region))));
    pw_stream_update_params(self->stream, params, 2);

    self->format_ready = true;
    pw_thread_loop_signal(self->loop, false);
}

void PipeWireVideoCapture::Impl::on_process(void* data) {
    auto* self = static_cast<Impl*>(data);
    // The newest buffer wins; older ones go straight back.
    pw_buffer* newest = nullptr;
    while (pw_buffer* b = pw_stream_dequeue_buffer(self->stream)) {
        if (newest) pw_stream_queue_buffer(self->stream, newest);
        newest = b;
    }
    if (!newest) return;

    spa_buffer* buf = newest->buffer;
    const spa_data& d = buf->datas[0];
    const bool usable = buf->n_datas > 0 && d.data && d.chunk && d.chunk->size > 0 &&
                        !(d.chunk->flags & SPA_CHUNK_FLAG_CORRUPTED);
    if (usable) {
        CpuFrame frame;
        frame.format = self->format;
        frame.width = self->width.load();
        frame.height = self->height.load();
        frame.stride = d.chunk->stride > 0 ? static_cast<uint32_t>(d.chunk->stride) : frame.width * 4;
        frame.data = static_cast<const uint8_t*>(d.data) + d.chunk->offset;
        if (auto* crop = static_cast<spa_meta_region*>(
                spa_buffer_find_meta_data(buf, SPA_META_VideoCrop, sizeof(spa_meta_region)))) {
            const spa_region& r = crop->region;
            if (r.size.width > 0 && r.size.height > 0 && r.position.x >= 0 && r.position.y >= 0 &&
                static_cast<uint32_t>(r.position.x) + r.size.width <= frame.width &&
                static_cast<uint32_t>(r.position.y) + r.size.height <= frame.height) {
                frame.data += static_cast<size_t>(r.position.y) * frame.stride + static_cast<size_t>(r.position.x) * 4;
                frame.width = r.size.width;
                frame.height = r.size.height;
            }
        }
        frame.width &= ~1u;
        frame.height &= ~1u;
        if (frame.width > 0 && frame.height > 0) {
            std::lock_guard<std::mutex> lock(self->callback_mutex);
            if (self->callback) {
                ++self->frames;
                self->callback(&frame, steady_us());
            }
        }
    }
    pw_stream_queue_buffer(self->stream, newest);
}

void PipeWireVideoCapture::Impl::teardown() {
    if (loop) pw_thread_loop_stop(loop);
    if (stream) {
        pw_stream_destroy(stream);
        stream = nullptr;
    }
    if (core) {
        pw_core_disconnect(core);
        core = nullptr;
    }
    if (context) {
        pw_context_destroy(context);
        context = nullptr;
    }
    if (loop) {
        pw_thread_loop_destroy(loop);
        loop = nullptr;
    }
}

PipeWireVideoCapture::PipeWireVideoCapture() : impl_(std::make_unique<Impl>()) {}

PipeWireVideoCapture::~PipeWireVideoCapture() {
    stop();
    impl_->teardown();
}

uint32_t PipeWireVideoCapture::width() const { return impl_->width.load(); }
uint32_t PipeWireVideoCapture::height() const { return impl_->height.load(); }
bool PipeWireVideoCapture::failed() const { return impl_->failed.load(); }

bool PipeWireVideoCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    (void)device;
    Impl& m = *impl_;
    if (desc.mode != CaptureMode::PipeWire || desc.pipewire_fd < 0 || desc.pipewire_node == 0) {
        MELLO_LOG_ERROR(TAG, "PipeWire screencast: no portal socket or node");
        return false;
    }
    mello::audio::pw::ensure_init();
    // pw_context_connect_fd takes the socket over; the caller keeps its own.
    const int fd = dup(desc.pipewire_fd);
    if (fd < 0) {
        MELLO_LOG_ERROR(TAG, "PipeWire screencast: dup of the portal socket failed");
        return false;
    }

    m.loop = pw_thread_loop_new("m3llo-screencast", nullptr);
    m.context = m.loop ? pw_context_new(pw_thread_loop_get_loop(m.loop), nullptr, 0) : nullptr;
    if (!m.context || pw_thread_loop_start(m.loop) < 0) {
        close(fd);
        MELLO_LOG_ERROR(TAG, "PipeWire screencast: no loop");
        m.teardown();
        return false;
    }

    pw_thread_loop_lock(m.loop);
    m.core = pw_context_connect_fd(m.context, fd, nullptr, 0);
    if (!m.core) {
        pw_thread_loop_unlock(m.loop);
        MELLO_LOG_ERROR(TAG, "PipeWire screencast: cannot connect over the portal socket");
        m.teardown();
        return false;
    }
    m.stream = pw_stream_new(m.core, "m3llo-screencast",
                             pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Capture",
                                               PW_KEY_MEDIA_ROLE, "Screen", nullptr));
    if (!m.stream) {
        pw_thread_loop_unlock(m.loop);
        m.teardown();
        return false;
    }
    pw_stream_add_listener(m.stream, &m.listener, &kStreamEvents, &m);

    // 32-bit RGB in any size and rate. No modifier is offered, so the
    // compositor sends shared memory, not DMA-BUF.
    uint8_t buffer[1024];
    spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
    spa_rectangle size_def{1920, 1080}, size_min{1, 1}, size_max{8192, 8192};
    spa_fraction rate_def{60, 1}, rate_min{0, 1}, rate_max{360, 1};
    const spa_pod* params[1];
    params[0] = static_cast<const spa_pod*>(spa_pod_builder_add_object(
        &b, SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat, SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
        SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw), SPA_FORMAT_VIDEO_format,
        SPA_POD_CHOICE_ENUM_Id(5, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRA,
                               SPA_VIDEO_FORMAT_RGBx, SPA_VIDEO_FORMAT_RGBA),
        SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(&size_def, &size_min, &size_max),
        SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(&rate_def, &rate_min, &rate_max)));

    const int rc = pw_stream_connect(m.stream, PW_DIRECTION_INPUT, desc.pipewire_node,
                                     static_cast<pw_stream_flags>(PW_STREAM_FLAG_AUTOCONNECT |
                                                                  PW_STREAM_FLAG_MAP_BUFFERS),
                                     params, 1);
    if (rc < 0) {
        pw_thread_loop_unlock(m.loop);
        MELLO_LOG_ERROR(TAG, "PipeWire screencast: connect to node %u failed: %d", desc.pipewire_node, rc);
        m.teardown();
        return false;
    }
    const auto deadline = std::chrono::steady_clock::now() + kFormatWait;
    while (!m.format_ready && !m.failed.load() && std::chrono::steady_clock::now() < deadline) {
        pw_thread_loop_timed_wait(m.loop, 1);
    }
    const bool ok = m.format_ready && !m.failed.load();
    pw_thread_loop_unlock(m.loop);
    if (!ok) {
        MELLO_LOG_ERROR(TAG, "PipeWire screencast: no format from node %u", desc.pipewire_node);
        m.teardown();
        return false;
    }
    MELLO_LOG_INFO(TAG, "PipeWire screencast ready: node %u %ux%u", desc.pipewire_node, m.width.load(),
                   m.height.load());
    return true;
}

bool PipeWireVideoCapture::start(uint32_t target_fps, FrameCallback callback) {
    (void)target_fps;  // the pipeline decimates; PipeWire sends what the compositor draws
    if (!impl_->stream) return false;
    std::lock_guard<std::mutex> lock(impl_->callback_mutex);
    impl_->callback = std::move(callback);
    return true;
}

void PipeWireVideoCapture::stop() {
    Impl& m = *impl_;
    if (m.loop && m.stream) {
        pw_thread_loop_lock(m.loop);
        pw_stream_disconnect(m.stream);
        pw_thread_loop_unlock(m.loop);
    }
    std::lock_guard<std::mutex> lock(m.callback_mutex);
    if (m.callback) {
        MELLO_LOG_INFO(TAG, "PipeWire screencast stopped after %llu pictures",
                       static_cast<unsigned long long>(m.frames));
    }
    m.callback = nullptr;
}

} // namespace mello::video
#endif
