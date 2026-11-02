#if defined(__linux__)
#include "capture_pipewire_video.hpp"
#include "capture_frame.hpp"
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

static uint32_t spa_format_of(uint32_t fourcc) {
    switch (fourcc) {
    case kDrmXRGB8888: return SPA_VIDEO_FORMAT_BGRx;
    case kDrmARGB8888: return SPA_VIDEO_FORMAT_BGRA;
    case kDrmXBGR8888: return SPA_VIDEO_FORMAT_RGBx;
    case kDrmABGR8888: return SPA_VIDEO_FORMAT_RGBA;
    default: return SPA_VIDEO_FORMAT_UNKNOWN;
    }
}

static uint32_t fourcc_of(uint32_t spa_format) {
    switch (spa_format) {
    case SPA_VIDEO_FORMAT_BGRx: return kDrmXRGB8888;
    case SPA_VIDEO_FORMAT_BGRA: return kDrmARGB8888;
    case SPA_VIDEO_FORMAT_RGBx: return kDrmXBGR8888;
    case SPA_VIDEO_FORMAT_RGBA: return kDrmABGR8888;
    default: return 0;
    }
}

// Buffers that left the loop thread inside a frame (DMA-BUF) come back here
// from whichever thread drops the last FrameRelease. The loop thread queues
// them to PipeWire on its event.
struct BufferReturn {
    std::mutex mutex;
    std::vector<pw_buffer*> pending;
    bool alive = true;
    pw_loop* loop = nullptr;
    spa_source* event = nullptr;
};

struct ReturnToken : FrameRelease {
    std::shared_ptr<BufferReturn> back;
    pw_buffer* buffer;
    ReturnToken(std::shared_ptr<BufferReturn> b, pw_buffer* buf) : back(std::move(b)), buffer(buf) {}
    ~ReturnToken() override {
        std::lock_guard<std::mutex> lock(back->mutex);
        if (!back->alive) return;  // the stream is gone, and its buffers with it
        back->pending.push_back(buffer);
        pw_loop_signal_event(back->loop, back->event);
    }
};

struct PipeWireVideoCapture::Impl {
    pw_thread_loop* loop = nullptr;
    pw_context* context = nullptr;
    pw_core* core = nullptr;
    pw_stream* stream = nullptr;
    spa_hook listener{};

    std::vector<DmaBufFormat> dmabuf_formats;  // what the encoder imports
    std::atomic<uint32_t> width{0};
    std::atomic<uint32_t> height{0};
    CpuPixelFormat format = CpuPixelFormat::BGRx;  // loop thread
    bool dmabuf = false;                            // loop thread
    uint32_t fourcc = 0;                            // loop thread
    uint64_t modifier = 0;                          // loop thread
    bool format_ready = false;                      // loop lock
    std::atomic<bool> failed{false};
    std::atomic<bool> streamed{false};
    std::shared_ptr<BufferReturn> returns = std::make_shared<BufferReturn>();

    std::mutex callback_mutex;
    FrameCallback callback;
    IdleCallback idle;
    uint64_t frames = 0;          // loop thread
    uint64_t frames_at_tick = 0;  // loop thread
    spa_source* idle_timer = nullptr;

    static void on_state_changed(void* data, pw_stream_state old, pw_stream_state state, const char* error);
    static void on_param_changed(void* data, uint32_t id, const spa_pod* param);
    static void on_process(void* data);
    static void on_idle_tick(void* data, uint64_t expirations);
    static void on_returns(void* data, uint64_t count);
    void teardown();
    /// EnumFormat params: one per DMA-BUF format (its modifiers, or only
    /// `fixed` when not 0), then shared memory. Into `b`.
    uint32_t build_formats(spa_pod_builder* b, const spa_pod** params, uint32_t max, uint64_t fixed_modifier,
                           uint32_t fixed_fourcc);
};

static const pw_stream_events kStreamEvents = [] {
    pw_stream_events e{};
    e.version = PW_VERSION_STREAM_EVENTS;
    e.state_changed = &PipeWireVideoCapture::Impl::on_state_changed;
    e.param_changed = &PipeWireVideoCapture::Impl::on_param_changed;
    e.process = &PipeWireVideoCapture::Impl::on_process;
    return e;
}();

static const spa_pod* format_param(spa_pod_builder* b, uint32_t spa_format, const std::vector<uint64_t>* modifiers) {
    spa_rectangle size_def{1920, 1080}, size_min{1, 1}, size_max{8192, 8192};
    spa_fraction rate_def{60, 1}, rate_min{0, 1}, rate_max{360, 1};
    spa_pod_frame f;
    spa_pod_builder_push_object(b, &f, SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat);
    spa_pod_builder_add(b, SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video), SPA_FORMAT_mediaSubtype,
                        SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw), SPA_FORMAT_VIDEO_format, SPA_POD_Id(spa_format), 0);
    if (modifiers && !modifiers->empty()) {
        spa_pod_builder_prop(b, SPA_FORMAT_VIDEO_modifier, SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE);
        spa_pod_frame choice;
        spa_pod_builder_push_choice(b, &choice, SPA_CHOICE_Enum, 0);
        spa_pod_builder_long(b, static_cast<int64_t>(modifiers->front()));  // the default
        for (uint64_t m : *modifiers) spa_pod_builder_long(b, static_cast<int64_t>(m));
        spa_pod_builder_pop(b, &choice);
    }
    spa_pod_builder_add(b, SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(&size_def, &size_min, &size_max),
                        SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(&rate_def, &rate_min, &rate_max),
                        0);
    return static_cast<const spa_pod*>(spa_pod_builder_pop(b, &f));
}

uint32_t PipeWireVideoCapture::Impl::build_formats(spa_pod_builder* b, const spa_pod** params, uint32_t max,
                                                   uint64_t fixed_modifier, uint32_t fixed_fourcc) {
    uint32_t n = 0;
    for (const DmaBufFormat& f : dmabuf_formats) {
        const uint32_t spa = spa_format_of(f.drm_fourcc);
        if (spa == SPA_VIDEO_FORMAT_UNKNOWN || f.modifiers.empty() || n + 1 >= max) continue;
        if (fixed_fourcc != 0) {
            if (f.drm_fourcc != fixed_fourcc) continue;
            const std::vector<uint64_t> one{fixed_modifier};
            params[n++] = format_param(b, spa, &one);
        } else {
            params[n++] = format_param(b, spa, &f.modifiers);
        }
    }
    // Shared memory, last: any 32-bit RGB.
    spa_rectangle size_def{1920, 1080}, size_min{1, 1}, size_max{8192, 8192};
    spa_fraction rate_def{60, 1}, rate_min{0, 1}, rate_max{360, 1};
    params[n++] = static_cast<const spa_pod*>(spa_pod_builder_add_object(
        b, SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat, SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
        SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw), SPA_FORMAT_VIDEO_format,
        SPA_POD_CHOICE_ENUM_Id(5, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRA,
                               SPA_VIDEO_FORMAT_RGBx, SPA_VIDEO_FORMAT_RGBA),
        SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(&size_def, &size_min, &size_max),
        SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(&rate_def, &rate_min, &rate_max)));
    return n;
}

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
    // A modifier property makes the format DMA-BUF. While it is still a
    // choice of several, the consumer fixes it. Read from the pod itself:
    // spa_video_info_raw.flags is newer than the build's PipeWire 0.3.48.
    bool dmabuf = false;
    bool needs_fixation = false;
    uint64_t modifier = 0;
    if (const spa_pod_prop* prop = spa_pod_find_prop(param, nullptr, SPA_FORMAT_VIDEO_modifier)) {
        dmabuf = true;
        uint32_t n_values = 0, choice = SPA_CHOICE_None;
        const spa_pod* value = spa_pod_get_values(&prop->value, &n_values, &choice);
        int64_t first = 0;
        if (value && spa_pod_get_long(value, &first) == 0) modifier = static_cast<uint64_t>(first);
        needs_fixation = choice != SPA_CHOICE_None && n_values > 1;
    }
    uint8_t buffer[4096];
    spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));

    if (needs_fixation) {
        // The compositor offers several modifiers: take its first choice.
        const spa_pod* params[8];
        const uint32_t n = self->build_formats(&b, params, 8, modifier, fourcc_of(info.format));
        MELLO_LOG_INFO(TAG, "PipeWire screencast: fixing the DMA-BUF modifier to 0x%llx",
                       static_cast<unsigned long long>(modifier));
        pw_stream_update_params(self->stream, params, n);
        return;
    }

    self->dmabuf = dmabuf;
    self->fourcc = fourcc_of(info.format);
    self->modifier = modifier;
    self->width.store(info.size.width);
    self->height.store(info.size.height);
    MELLO_LOG_INFO(TAG, "PipeWire screencast: %ux%u %s, %s, %u/%u fps", info.size.width, info.size.height,
                   self->format == CpuPixelFormat::BGRx ? "BGRx" : "RGBx",
                   dmabuf ? "DMA-BUF (GPU memory)" : "shared memory", info.max_framerate.num,
                   info.max_framerate.denom);
    if (dmabuf) {
        MELLO_LOG_INFO(TAG, "PipeWire screencast: DMA-BUF modifier 0x%llx",
                       static_cast<unsigned long long>(self->modifier));
    }

    const int data_types = dmabuf ? (1 << SPA_DATA_DmaBuf) : ((1 << SPA_DATA_MemPtr) | (1 << SPA_DATA_MemFd));
    const spa_pod* params[2];
    params[0] = static_cast<const spa_pod*>(spa_pod_builder_add_object(
        &b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers, SPA_PARAM_BUFFERS_buffers,
        SPA_POD_CHOICE_RANGE_Int(6, 4, 16), SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(data_types)));
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
    const bool is_dmabuf = buf->n_datas > 0 && d.type == SPA_DATA_DmaBuf && d.fd >= 0;
    const bool usable = buf->n_datas > 0 && d.chunk && !(d.chunk->flags & SPA_CHUNK_FLAG_CORRUPTED) &&
                        (is_dmabuf || (d.data && d.chunk->size > 0));
    if (!usable) {
        pw_stream_queue_buffer(self->stream, newest);
        return;
    }

    CaptureFrame frame;
    frame.format = self->format;
    const uint32_t full_w = self->width.load();
    const uint32_t full_h = self->height.load();
    frame.width = full_w;
    frame.height = full_h;
    frame.stride = d.chunk->stride > 0 ? static_cast<uint32_t>(d.chunk->stride) : full_w * 4;
    uint32_t crop_x = 0, crop_y = 0;
    if (auto* crop = static_cast<spa_meta_region*>(
            spa_buffer_find_meta_data(buf, SPA_META_VideoCrop, sizeof(spa_meta_region)))) {
        const spa_region& r = crop->region;
        if (r.size.width > 0 && r.size.height > 0 && r.position.x >= 0 && r.position.y >= 0 &&
            static_cast<uint32_t>(r.position.x) + r.size.width <= full_w &&
            static_cast<uint32_t>(r.position.y) + r.size.height <= full_h) {
            crop_x = static_cast<uint32_t>(r.position.x);
            crop_y = static_cast<uint32_t>(r.position.y);
            frame.width = r.size.width;
            frame.height = r.size.height;
        }
    }
    frame.width &= ~1u;
    frame.height &= ~1u;

    if (is_dmabuf) {
        frame.dmabuf_fd = static_cast<int>(d.fd);
        frame.dmabuf_offset = d.chunk->offset;
        frame.dmabuf_size = d.maxsize;
        frame.drm_fourcc = self->fourcc;
        frame.modifier = self->modifier;
        frame.buffer_width = full_w;
        frame.buffer_height = full_h;
        frame.crop_x = crop_x;
        frame.crop_y = crop_y;
        // The buffer goes back when the GPU no longer reads it.
        frame.release = std::make_shared<ReturnToken>(self->returns, newest);
    } else {
        frame.data = static_cast<const uint8_t*>(d.data) + d.chunk->offset +
                     static_cast<size_t>(crop_y) * frame.stride + static_cast<size_t>(crop_x) * 4;
    }

    if (frame.width > 0 && frame.height > 0) {
        std::lock_guard<std::mutex> lock(self->callback_mutex);
        if (self->callback) {
            ++self->frames;
            self->callback(&frame, steady_us());
        }
    }
    if (!is_dmabuf) pw_stream_queue_buffer(self->stream, newest);
    // A DMA-BUF frame returns its buffer through ReturnToken: now, if no one
    // kept the frame, or when the encoder lets go of it.
}

void PipeWireVideoCapture::Impl::on_returns(void* data, uint64_t count) {
    (void)count;
    auto* self = static_cast<Impl*>(data);
    std::vector<pw_buffer*> back;
    {
        std::lock_guard<std::mutex> lock(self->returns->mutex);
        back.swap(self->returns->pending);
    }
    for (pw_buffer* b : back) pw_stream_queue_buffer(self->stream, b);
}

void PipeWireVideoCapture::Impl::on_idle_tick(void* data, uint64_t expirations) {
    (void)expirations;
    auto* self = static_cast<Impl*>(data);
    std::lock_guard<std::mutex> lock(self->callback_mutex);
    const bool still = self->frames == self->frames_at_tick;
    self->frames_at_tick = self->frames;
    if (still && self->idle) self->idle();
}

void PipeWireVideoCapture::Impl::teardown() {
    {
        // Frames still held elsewhere must not queue buffers of a stream
        // that is gone.
        std::lock_guard<std::mutex> lock(returns->mutex);
        returns->alive = false;
        returns->pending.clear();
    }
    if (loop) {
        pw_thread_loop_lock(loop);
        pw_loop* l = pw_thread_loop_get_loop(loop);
        if (idle_timer) pw_loop_destroy_source(l, idle_timer);
        if (returns->event) pw_loop_destroy_source(l, returns->event);
        idle_timer = nullptr;
        returns->event = nullptr;
        pw_thread_loop_unlock(loop);
        pw_thread_loop_stop(loop);
    }
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
    m.dmabuf_formats = desc.dmabuf_formats;
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
    m.returns->loop = pw_thread_loop_get_loop(m.loop);
    m.returns->event = pw_loop_add_event(m.returns->loop, &Impl::on_returns, &m);
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

    // DMA-BUF in the layouts the encoder imports first, so the picture stays
    // on the GPU; shared memory when the compositor shares none of them.
    uint8_t buffer[4096];
    spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
    const spa_pod* params[8];
    const uint32_t n = m.build_formats(&b, params, 8, 0, 0);
    MELLO_LOG_INFO(TAG, "PipeWire screencast: offering %u DMA-BUF format(s) and shared memory", n - 1);

    const int rc = pw_stream_connect(m.stream, PW_DIRECTION_INPUT, desc.pipewire_node,
                                     static_cast<pw_stream_flags>(PW_STREAM_FLAG_AUTOCONNECT |
                                                                  PW_STREAM_FLAG_MAP_BUFFERS),
                                     params, n);
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

void PipeWireVideoCapture::set_idle_callback(IdleCallback cb) {
    Impl& m = *impl_;
    {
        std::lock_guard<std::mutex> lock(m.callback_mutex);
        m.idle = std::move(cb);
    }
    if (!m.loop || m.idle_timer) return;
    pw_thread_loop_lock(m.loop);
    pw_loop* loop = pw_thread_loop_get_loop(m.loop);
    m.idle_timer = pw_loop_add_timer(loop, &Impl::on_idle_tick, &m);
    if (m.idle_timer) {
        timespec every{0, 20 * 1000 * 1000};
        pw_loop_update_timer(loop, m.idle_timer, &every, &every, false);
    }
    pw_thread_loop_unlock(m.loop);
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
    {
        // Frames the encoder still holds no longer go back: the stream's
        // buffers go with the disconnect. The encoder holds its own
        // duplicate of each DMA-BUF descriptor.
        std::lock_guard<std::mutex> lock(m.returns->mutex);
        m.returns->alive = false;
        m.returns->pending.clear();
    }
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
    m.idle = nullptr;
}

} // namespace mello::video
#endif
