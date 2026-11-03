#include "clip_player.hpp"
#include "h264_annex_b.hpp"
#include "../util/log.hpp"

#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <wrl/client.h>

#include <algorithm>
#include <atomic>
#include <condition_variable>
#include <cstring>
#include <deque>
#include <mutex>
#include <optional>
#include <thread>
#include <vector>

namespace mello::clip {

using Microsoft::WRL::ComPtr;
using transport::TimelineDecision;
using transport::TimelineFrame;
using video::ViewerPipeline;

namespace {

constexpr const char* TAG = "clip/player";

/// A frame leaves the source this long before its render time: one decode,
/// and no more, or the 3-slot ring evicts frames that wait.
constexpr int64_t kDecodeLeadUs = 25'000;
/// How often a source with nothing to release looks again.
constexpr int64_t kIdlePollUs = 10'000;
/// Sound that must wait before play starts (0.3 s) and goes on after a stall
/// (1 s, plan 6.2); under 50 ms of it is a stall.
constexpr size_t kStartFrames = audio::ClipAudioStream::kSampleRate * 3 / 10;
constexpr size_t kResumeFrames = audio::ClipAudioStream::kSampleRate;
constexpr size_t kStarveFrames = audio::ClipAudioStream::kSampleRate / 20;
/// Demuxed frames that may wait: 4 s at 60 fps, 10 s while the sound starves
/// (an MP4 can put a second of video before the sound of that second).
constexpr size_t kFrameCap = 240;
constexpr size_t kFrameCapStarving = 600;

int64_t now_us() { return ViewerPipeline::now_us(); }

// ---- Byte stream ------------------------------------------------------------

/// One BeginRead: where, how much, and (when done) the bytes read, for
/// EndRead. The object of the caller's async result.
class PendingRead : public IUnknown {
public:
    PendingRead(QWORD at, BYTE* buf, ULONG len) : at(at), buf(buf), len(len) {}
    STDMETHODIMP QueryInterface(REFIID riid, void** ppv) override {
        if (!ppv) return E_POINTER;
        if (riid == __uuidof(IUnknown)) {
            *ppv = static_cast<IUnknown*>(this);
            AddRef();
            return S_OK;
        }
        *ppv = nullptr;
        return E_NOINTERFACE;
    }
    STDMETHODIMP_(ULONG) AddRef() override { return ++refs_; }
    STDMETHODIMP_(ULONG) Release() override {
        const ULONG r = --refs_;
        if (r == 0) delete this;
        return r;
    }
    const QWORD at;
    BYTE* const buf;
    const ULONG len;
    ULONG got = 0;

private:
    std::atomic<ULONG> refs_{1};
};

/// The MP4 as Media Foundation reads it: every read goes to the read
/// callback. A read that waits for bytes asks again every 15 ms until they
/// are there or the player stops. Called on Media Foundation threads.
///
/// BeginRead returns at once and reads on Media Foundation's long-function
/// work queue. A read that waited on the source's own thread kept the MPEG-4
/// source from answering sample requests: over a slow link it read 3 to 6 MiB
/// ahead before it gave the first sample (plans/GAME-CLIPS.md 10.3).
class ClipByteStream : public IMFByteStream, public IMFAsyncCallback {
public:
    ClipByteStream(uint64_t length, ReadFn read, std::shared_ptr<std::atomic<bool>> stop,
                   std::shared_ptr<std::atomic<bool>> waiting)
        : length_(length), read_(std::move(read)), stop_(std::move(stop)), waiting_(std::move(waiting)) {}

    STDMETHODIMP QueryInterface(REFIID riid, void** ppv) override {
        if (!ppv) return E_POINTER;
        if (riid == __uuidof(IUnknown) || riid == __uuidof(IMFByteStream)) {
            *ppv = static_cast<IMFByteStream*>(this);
            AddRef();
            return S_OK;
        }
        if (riid == __uuidof(IMFAsyncCallback)) {
            *ppv = static_cast<IMFAsyncCallback*>(this);
            AddRef();
            return S_OK;
        }
        *ppv = nullptr;
        return E_NOINTERFACE;
    }
    STDMETHODIMP_(ULONG) AddRef() override { return ++refs_; }
    STDMETHODIMP_(ULONG) Release() override {
        const ULONG r = --refs_;
        if (r == 0) delete this;
        return r;
    }

    /// Ends every read: a read that waits returns E_ABORT, and this waits
    /// until no read runs. The demux thread calls it before it shuts the
    /// source down, so no read calls back into a source that is gone.
    void abort_reads() {
        closing_.store(true, std::memory_order_release);
        std::unique_lock<std::mutex> lock(idle_mutex_);
        idle_cv_.wait(lock, [this] { return in_flight_ == 0; });
    }

    STDMETHODIMP GetCapabilities(DWORD* caps) override {
        if (!caps) return E_POINTER;
        *caps = MFBYTESTREAM_IS_READABLE | MFBYTESTREAM_IS_SEEKABLE;
        return S_OK;
    }
    STDMETHODIMP GetLength(QWORD* length) override {
        if (!length) return E_POINTER;
        *length = length_;
        return S_OK;
    }
    STDMETHODIMP SetLength(QWORD) override { return E_NOTIMPL; }
    STDMETHODIMP GetCurrentPosition(QWORD* position) override {
        if (!position) return E_POINTER;
        std::lock_guard<std::mutex> lock(mutex_);
        *position = position_;
        return S_OK;
    }
    STDMETHODIMP SetCurrentPosition(QWORD position) override {
        std::lock_guard<std::mutex> lock(mutex_);
        position_ = std::min<QWORD>(position, length_);
        return S_OK;
    }
    STDMETHODIMP IsEndOfStream(BOOL* end) override {
        if (!end) return E_POINTER;
        std::lock_guard<std::mutex> lock(mutex_);
        *end = position_ >= length_;
        return S_OK;
    }
    STDMETHODIMP Read(BYTE* buf, ULONG len, ULONG* read) override {
        QWORD at = 0;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            at = position_;
        }
        ULONG got = 0;
        const HRESULT hr = read_at(at, buf, len, &got);
        if (SUCCEEDED(hr)) {
            std::lock_guard<std::mutex> lock(mutex_);
            position_ = at + got;
        }
        if (read) *read = got;
        return hr;
    }
    STDMETHODIMP BeginRead(BYTE* buf, ULONG len, IMFAsyncCallback* callback, IUnknown* state) override {
        if (!buf || !callback) return E_POINTER;
        QWORD at = 0;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            at = position_;
        }
        ComPtr<IUnknown> op;
        op.Attach(new PendingRead(at, buf, len));
        ComPtr<IMFAsyncResult> caller;
        HRESULT hr = MFCreateAsyncResult(op.Get(), callback, state, &caller);
        if (FAILED(hr)) return hr;
        {
            std::lock_guard<std::mutex> lock(idle_mutex_);
            ++in_flight_;
        }
        // The caller's result is the work item's state: Invoke reads into
        // its PendingRead and then calls the caller back.
        hr = MFPutWorkItem2(MFASYNC_CALLBACK_QUEUE_LONG_FUNCTION, 0, this, caller.Get());
        if (FAILED(hr)) read_done();
        return hr;
    }
    STDMETHODIMP EndRead(IMFAsyncResult* result, ULONG* read) override {
        if (!result || !read) return E_POINTER;
        ComPtr<IUnknown> op;
        result->GetObject(&op);
        *read = op ? static_cast<PendingRead*>(op.Get())->got : 0;
        return result->GetStatus();
    }

    // IMFAsyncCallback: one queued read, on the long-function work queue.
    STDMETHODIMP GetParameters(DWORD*, DWORD*) override { return E_NOTIMPL; }
    STDMETHODIMP Invoke(IMFAsyncResult* work) override {
        ComPtr<IUnknown> state;
        ComPtr<IMFAsyncResult> caller;
        ComPtr<IUnknown> op;
        if (work && SUCCEEDED(work->GetState(&state)) && SUCCEEDED(state.As(&caller)) &&
            SUCCEEDED(caller->GetObject(&op))) {
            auto* read = static_cast<PendingRead*>(op.Get());
            const HRESULT hr = read_at(read->at, read->buf, read->len, &read->got);
            if (SUCCEEDED(hr)) {
                std::lock_guard<std::mutex> lock(mutex_);
                position_ = read->at + read->got;
            }
            caller->SetStatus(hr);
            MFInvokeCallback(caller.Get());
        }
        read_done();
        return S_OK;
    }
    STDMETHODIMP Write(const BYTE*, ULONG, ULONG*) override { return E_NOTIMPL; }
    STDMETHODIMP BeginWrite(const BYTE*, ULONG, IMFAsyncCallback*, IUnknown*) override { return E_NOTIMPL; }
    STDMETHODIMP EndWrite(IMFAsyncResult*, ULONG*) override { return E_NOTIMPL; }
    STDMETHODIMP Seek(MFBYTESTREAM_SEEK_ORIGIN origin, LONGLONG offset, DWORD, QWORD* current) override {
        std::lock_guard<std::mutex> lock(mutex_);
        const LONGLONG base = origin == msoCurrent ? static_cast<LONGLONG>(position_) : 0;
        const LONGLONG at = std::clamp<LONGLONG>(base + offset, 0, static_cast<LONGLONG>(length_));
        position_ = static_cast<QWORD>(at);
        if (current) *current = position_;
        return S_OK;
    }
    STDMETHODIMP Flush() override { return S_OK; }
    STDMETHODIMP Close() override { return S_OK; }

private:
    void read_done() {
        {
            std::lock_guard<std::mutex> lock(idle_mutex_);
            --in_flight_;
        }
        idle_cv_.notify_all();
    }

    HRESULT read_at(QWORD at, BYTE* buf, ULONG len, ULONG* got) {
        while (*got < len && at + *got < length_) {
            if (stop_->load(std::memory_order_acquire) || closing_.load(std::memory_order_acquire)) {
                waiting_->store(false);
                return E_ABORT;
            }
            const int64_t r = read_(at + *got, buf + *got, len - *got);
            if (r == kReadWait) {
                waiting_->store(true);
                Sleep(15);
                continue;
            }
            if (r < 0) {
                waiting_->store(false);
                MELLO_LOG_WARN(TAG, "read of %lu bytes at %llu failed (%lld)", len - *got,
                               static_cast<unsigned long long>(at + *got), static_cast<long long>(r));
                return E_FAIL;
            }
            if (r == 0) break;
            *got += static_cast<ULONG>(r);
        }
        waiting_->store(false);
        return S_OK;
    }

    const uint64_t length_;
    ReadFn read_;
    std::shared_ptr<std::atomic<bool>> stop_;
    std::shared_ptr<std::atomic<bool>> waiting_;
    std::mutex mutex_;
    QWORD position_ = 0;
    std::atomic<bool> closing_{false};
    std::mutex idle_mutex_;
    std::condition_variable idle_cv_;
    int in_flight_ = 0;  // idle_mutex_
    std::atomic<ULONG> refs_{1};
};

// ---- Frame source -----------------------------------------------------------

struct DemuxedFrame {
    std::vector<uint8_t> annex_b;
    int64_t pts_us = 0;
    bool keyframe = false;
};

/// The demuxed frames, released to the viewer's decode thread at their
/// render times. The player says the render time of a frame's time, or
/// nothing while the picture holds.
class ClipFrameSource : public transport::FrameSource {
public:
    /// `first`: the first frame since the start or a seek. It shows at once,
    /// so the picture never waits for the sound.
    using RenderTimeFn = std::function<std::optional<int64_t>(int64_t pts_us, int64_t now, bool first)>;

    explicit ClipFrameSource(RenderTimeFn render_time) : render_time_(std::move(render_time)) {}

    TimelineDecision poll() override {
        TimelineDecision d;
        const int64_t now = now_us();
        std::unique_lock<std::mutex> lock(mutex_);
        if (stopped_ || frames_.empty()) {
            d.kind = TimelineDecision::Kind::Wait;
            d.wake_us = now + kIdlePollUs;
            return d;
        }
        const int64_t pts = frames_.front().pts_us;
        const uint64_t clears = clears_;
        const bool first = released_ == 0;
        lock.unlock();
        const std::optional<int64_t> render = render_time_(pts, now, first);
        lock.lock();
        // A seek cleared the queue meanwhile: this time is not for its front.
        if (frames_.empty() || !render || clears_ != clears) {
            d.kind = TimelineDecision::Kind::Wait;
            d.wake_us = now + kIdlePollUs;
            return d;
        }
        if (*render - now > kDecodeLeadUs) {
            d.kind = TimelineDecision::Kind::Wait;
            d.wake_us = std::min(*render - kDecodeLeadUs, now + 100'000);
            return d;
        }
        DemuxedFrame f = std::move(frames_.front());
        frames_.pop_front();
        ++released_;
        last_released_pts_ = f.pts_us;
        lock.unlock();
        space_cv_.notify_all();
        d.kind = TimelineDecision::Kind::Release;
        d.frame.annex_b = std::move(f.annex_b);
        d.frame.keyframe = f.keyframe;
        d.frame.render_time_us = *render;
        d.frame.released_us = now;
        return d;
    }
    void on_decoded(int64_t) override {}
    void on_decode_error() override { decode_errors_.fetch_add(1); }
    bool wait(int64_t wake_us) override {
        std::unique_lock<std::mutex> lock(mutex_);
        const int64_t now = now_us();
        const int64_t until = wake_us > 0 ? wake_us : now + 100'000;
        if (until > now && !stopped_) {
            frame_cv_.wait_for(lock, std::chrono::microseconds(until - now));
        }
        return !stopped_;
    }
    void wake() override { frame_cv_.notify_all(); }
    transport::TimelineStats stats() const override {
        transport::TimelineStats s;
        std::lock_guard<std::mutex> lock(mutex_);
        s.frames_released = released_total_ + released_;
        s.frames_buffered = frames_.size();
        return s;
    }

    // ---- Demux thread ----

    /// Waits while `cap` frames wait, then adds the frame. False when
    /// `cancel` turned true while it waited.
    bool push(DemuxedFrame frame, const std::function<size_t()>& cap,
              const std::function<bool()>& cancel) {
        std::unique_lock<std::mutex> lock(mutex_);
        while (frames_.size() >= cap()) {
            if (cancel()) return false;
            space_cv_.wait_for(lock, std::chrono::milliseconds(10));
        }
        frames_.push_back(std::move(frame));
        lock.unlock();
        frame_cv_.notify_all();
        return true;
    }
    /// A seek: the frames that wait are gone, and the next one is a first.
    void clear() {
        std::lock_guard<std::mutex> lock(mutex_);
        frames_.clear();
        released_total_ += released_;
        released_ = 0;
        ++clears_;
        space_cv_.notify_all();
    }
    void stop() {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            stopped_ = true;
        }
        frame_cv_.notify_all();
        space_cv_.notify_all();
    }
    void wake_pusher() { space_cv_.notify_all(); }

    size_t waiting() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return frames_.size();
    }
    int64_t last_released_pts() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return last_released_pts_;
    }
    uint64_t decode_errors() const { return decode_errors_.load(); }

private:
    RenderTimeFn render_time_;
    mutable std::mutex mutex_;
    std::condition_variable frame_cv_;
    std::condition_variable space_cv_;
    std::deque<DemuxedFrame> frames_;
    uint64_t released_ = 0;        // since the last clear
    uint64_t released_total_ = 0;  // before it
    uint64_t clears_ = 0;
    int64_t last_released_pts_ = 0;
    bool stopped_ = false;
    std::atomic<uint64_t> decode_errors_{0};
};

// ---- Player -----------------------------------------------------------------

class WinClipPlayer final : public ClipPlayer {
public:
    explicit WinClipPlayer(ClipPlayerOptions options) : options_(std::move(options)) {
        audio_ = std::make_shared<audio::ClipAudioStream>();
        source_ = std::make_shared<ClipFrameSource>(
            [this](int64_t pts, int64_t now, bool first) { return render_time(pts, now, first); });
        thread_ = std::thread([this] { demux_main(); });
    }
    ~WinClipPlayer() override { stop(); }

    void set_native_frame_callback(ViewerPipeline::NativeFrameCallback cb) override {
        std::lock_guard<std::mutex> lock(viewer_mutex_);
        native_cb_ = cb;
        if (viewer_) viewer_->set_native_frame_callback(std::move(cb));
    }

    void set_paused(bool paused) override {
        std::lock_guard<std::mutex> lock(clock_mutex_);
        if (paused == paused_) return;
        const int64_t now = now_us();
        if (!has_audio_ || audio_done_) {
            wall_pos_ = position_locked(now);
            wall_at_ = now;
        }
        paused_ = paused;
        apply_audio_pause_locked();
        MELLO_LOG_INFO(TAG, "%s at %.2f s", paused ? "paused" : "playing", position_locked(now) / 1e6);
    }

    void seek(int64_t position_us) override {
        seek_to_.store(std::max<int64_t>(0, position_us));
        seek_pending_.store(true, std::memory_order_release);
        source_->wake_pusher();
    }

    ClipPlayerState state() const override {
        ClipPlayerState s;
        const int64_t now = now_us();
        std::lock_guard<std::mutex> lock(clock_mutex_);
        s.duration_us = duration_us_.load();
        s.position_us = std::clamp<int64_t>(position_locked(now), 0, std::max<int64_t>(s.duration_us, 0));
        s.width = width_.load();
        s.height = height_.load();
        s.paused = paused_;
        s.buffering = reports_buffering(buffering_, has_audio_, waiting_->load(), ended_locked(now));
        s.ended = ended_locked(now);
        s.failed = failed_.load();
        return s;
    }

    void stop() override {
        if (stopped_.exchange(true)) return;
        stop_flag_->store(true, std::memory_order_release);
        source_->stop();
        if (thread_.joinable()) thread_.join();
    }

private:
    // ---- Clock (clock_mutex_) ----

    int64_t position_locked(int64_t now) const {
        if (!has_audio_ || audio_done_) {
            return wall_pos_ + (paused_ || buffering_ ? 0 : now - wall_at_);
        }
        return audio_position(now);
    }

    int64_t audio_position(int64_t now) const {
        const int64_t played = static_cast<int64_t>(audio_->played_frames() * 1'000'000 /
                                                    audio::ClipAudioStream::kSampleRate);
        int64_t since = 0;
        const int64_t mixed_at = audio_->last_mix_us();
        if (!audio_->paused() && audio_->buffered_frames() > 0 && mixed_at > 0) {
            since = std::clamp<int64_t>(now - mixed_at, 0, 20'000);
        }
        return played + since - latency_us_;
    }

    bool ended_locked(int64_t now) const {
        return demux_ended_.load() && source_->waiting() == 0 &&
               (!has_audio_ || audio_->buffered_frames() == 0) &&
               position_locked(now) >= source_->last_released_pts();
    }

    void apply_audio_pause_locked() { audio_->set_paused(paused_ || buffering_); }

    /// The decode thread asks this before each release: it also runs the
    /// stall rule and the change to the wall clock when the sound ends.
    std::optional<int64_t> render_time(int64_t pts, int64_t now, bool first) {
        std::lock_guard<std::mutex> lock(clock_mutex_);
        if (has_audio_ && !audio_done_ && audio_->ended() && audio_->buffered_frames() == 0) {
            // The sound is over: the last frames follow the wall clock.
            wall_pos_ = audio_position(now);
            wall_at_ = now;
            audio_done_ = true;
        }
        if (has_audio_ && !audio_done_) {
            const size_t buffered = audio_->buffered_frames();
            const bool eos = demux_ended_.load() || audio_->ended();
            if (!buffering_ && started_ && buffered < kStarveFrames && !eos && !paused_) {
                buffering_ = true;
                MELLO_LOG_INFO(TAG, "buffering at %.2f s", audio_position(now) / 1e6);
                apply_audio_pause_locked();
            } else if (buffering_ && (buffered >= (started_ ? kResumeFrames : kStartFrames) || eos)) {
                buffering_ = false;
                if (started_) MELLO_LOG_INFO(TAG, "playing again at %.2f s", audio_position(now) / 1e6);
                started_ = true;
                apply_audio_pause_locked();
            }
        } else if (buffering_) {
            buffering_ = false;
            started_ = true;
            wall_at_ = now;
        }
        if (first) return now;
        if (paused_ || buffering_) return std::nullopt;
        return now + (pts - position_locked(now));
    }

    // ---- Demux thread ----

    void demux_main() {
        const HRESULT co = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        const HRESULT mf = MFStartup(MF_VERSION);
        if (FAILED(mf)) {
            MELLO_LOG_ERROR(TAG, "MFStartup failed: hr=0x%08X", static_cast<unsigned>(mf));
            failed_.store(true);
        } else {
            run();
            MFShutdown();
        }
        if (SUCCEEDED(co)) CoUninitialize();
    }

    bool open() {
        byte_stream_.Attach(new ClipByteStream(options_.length, options_.read, stop_flag_, waiting_));
        ComPtr<IMFSourceResolver> resolver;
        HRESULT hr = MFCreateSourceResolver(&resolver);
        MF_OBJECT_TYPE type = MF_OBJECT_INVALID;
        ComPtr<IUnknown> object;
        if (SUCCEEDED(hr)) {
            hr = resolver->CreateObjectFromByteStream(
                static_cast<IMFByteStream*>(byte_stream_.Get()), L"clip.mp4",
                MF_RESOLUTION_MEDIASOURCE | MF_RESOLUTION_CONTENT_DOES_NOT_HAVE_TO_MATCH_EXTENSION_OR_MIME_TYPE,
                nullptr, &type, &object);
        }
        if (SUCCEEDED(hr)) hr = object.As(&media_source_);
        // No D3D manager and no video processing: the H.264 stays compressed
        // for the viewer's decoder, and Media Foundation makes no device.
        if (SUCCEEDED(hr)) hr = MFCreateSourceReaderFromMediaSource(media_source_.Get(), nullptr, &reader_);
        if (FAILED(hr)) {
            if (!stop_flag_->load()) {
                MELLO_LOG_ERROR(TAG, "the clip does not open: hr=0x%08X", static_cast<unsigned>(hr));
            }
            return false;
        }

        // The streams.
        for (DWORD i = 0;; ++i) {
            ComPtr<IMFMediaType> native;
            if (FAILED(reader_->GetNativeMediaType(i, 0, &native))) break;
            GUID major{};
            native->GetMajorType(&major);
            if (major == MFMediaType_Video && video_index_ == kNone) {
                video_index_ = i;
                UINT32 w = 0, h = 0;
                MFGetAttributeSize(native.Get(), MF_MT_FRAME_SIZE, &w, &h);
                width_.store(w);
                height_.store(h);
                UINT32 blob = 0;
                if (SUCCEEDED(native->GetBlobSize(MF_MT_MPEG_SEQUENCE_HEADER, &blob)) && blob > 0) {
                    sequence_header_.resize(blob);
                    native->GetBlob(MF_MT_MPEG_SEQUENCE_HEADER, sequence_header_.data(), blob, nullptr);
                    sequence_header_ = to_annex_b(std::move(sequence_header_));
                }
            } else if (major == MFMediaType_Audio && audio_index_ == kNone) {
                audio_index_ = i;
            }
        }
        if (video_index_ == kNone) {
            MELLO_LOG_ERROR(TAG, "the clip has no video");
            return false;
        }
        reader_->SetStreamSelection(static_cast<DWORD>(MF_SOURCE_READER_ALL_STREAMS), FALSE);
        reader_->SetStreamSelection(video_index_, TRUE);
        if (audio_index_ != kNone) {
            ComPtr<IMFMediaType> pcm;
            MFCreateMediaType(&pcm);
            pcm->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
            pcm->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_PCM);
            pcm->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
            pcm->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, audio::ClipAudioStream::kSampleRate);
            pcm->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, audio::ClipAudioStream::kChannels);
            if (SUCCEEDED(reader_->SetCurrentMediaType(audio_index_, nullptr, pcm.Get()))) {
                reader_->SetStreamSelection(audio_index_, TRUE);
            } else {
                MELLO_LOG_WARN(TAG, "the clip's sound does not decode to 48 kHz stereo: no sound");
                audio_index_ = kNone;
            }
        }

        PROPVARIANT var;
        PropVariantInit(&var);
        if (SUCCEEDED(reader_->GetPresentationAttribute(static_cast<DWORD>(MF_SOURCE_READER_MEDIASOURCE),
                                                        MF_PD_DURATION, &var))) {
            duration_us_.store(static_cast<int64_t>(var.uhVal.QuadPart / 10));
        }
        PropVariantClear(&var);
        return true;
    }

    bool start_viewer(const video::GraphicsDevice& device) {
        auto viewer = std::make_unique<ViewerPipeline>();
        video::ViewerConfig config;
        config.width = width_.load();
        config.height = height_.load();
        config.fps = 60;
        std::unique_ptr<video::Decoder> decoder =
            options_.make_decoder ? options_.make_decoder(device) : nullptr;
        if (!viewer->start(device, config, options_.on_cpu_frame, std::move(decoder))) {
            MELLO_LOG_ERROR(TAG, "no decoder for the clip (%ux%u)", config.width, config.height);
            return false;
        }
        std::lock_guard<std::mutex> lock(viewer_mutex_);
        if (native_cb_) viewer->set_native_frame_callback(native_cb_);
        auto source = source_;
        viewer->attach_timeline([source] { return source; });
        viewer_ = std::move(viewer);
        return true;
    }

    void run() {
        if (!open()) {
            failed_.store(!stop_flag_->load());
            close_source();
            return;
        }
        {
            std::lock_guard<std::mutex> lock(clock_mutex_);
            has_audio_ = audio_index_ != kNone;
            latency_us_ = options_.output_latency_us ? options_.output_latency_us() : 0;
            buffering_ = has_audio_;
            wall_at_ = now_us();
        }
        video::GraphicsDevice device = video::create_d3d11_device();
        if (!device.handle || !start_viewer(device)) {
            failed_.store(true);
        } else {
            if (has_audio_ && options_.attach_audio) options_.attach_audio(audio_);
            MELLO_LOG_INFO(TAG, "playing: %ux%u, %.2f s, sound %s, %llu bytes", width_.load(), height_.load(),
                           duration_us_.load() / 1e6, has_audio_ ? "on" : "off",
                           static_cast<unsigned long long>(options_.length));
            demux_loop();
            if (has_audio_ && options_.attach_audio) options_.attach_audio(nullptr);
        }
        source_->stop();
        {
            std::unique_ptr<ViewerPipeline> viewer;
            {
                std::lock_guard<std::mutex> lock(viewer_mutex_);
                viewer = std::move(viewer_);
            }
            if (viewer) viewer->stop();
        }
        if (device.handle) device.d3d11()->Release();
        close_source();
    }

    void close_source() {
        if (byte_stream_) byte_stream_->abort_reads();
        reader_.Reset();
        if (media_source_) media_source_->Shutdown();
        media_source_.Reset();
    }

    bool cancelled() const {
        return stop_flag_->load(std::memory_order_acquire) || seek_pending_.load(std::memory_order_acquire);
    }

    void demux_loop() {
        bool video_eos = false;
        bool audio_eos = audio_index_ == kNone;
        bool want_header = true;
        while (!stop_flag_->load(std::memory_order_acquire)) {
            if (seek_pending_.exchange(false, std::memory_order_acq_rel)) {
                do_seek(seek_to_.load());
                video_eos = false;
                audio_eos = audio_index_ == kNone;
                want_header = true;
            }
            if (video_eos && audio_eos) {
                std::this_thread::sleep_for(std::chrono::milliseconds(20));
                continue;
            }
            DWORD index = 0;
            DWORD flags = 0;
            LONGLONG time = 0;
            ComPtr<IMFSample> sample;
            const HRESULT hr = reader_->ReadSample(static_cast<DWORD>(MF_SOURCE_READER_ANY_STREAM), 0, &index,
                                                   &flags, &time, &sample);
            if (stop_flag_->load()) break;
            if (FAILED(hr) || (flags & MF_SOURCE_READERF_ERROR)) {
                MELLO_LOG_ERROR(TAG, "the clip does not read: hr=0x%08X", static_cast<unsigned>(hr));
                failed_.store(true);
                break;
            }
            if (flags & MF_SOURCE_READERF_ENDOFSTREAM) {
                if (index == video_index_) video_eos = true;
                if (index == audio_index_) {
                    audio_eos = true;
                    audio_->set_ended(true);
                }
                if (video_eos && audio_eos) {
                    demux_ended_.store(true);
                    MELLO_LOG_INFO(TAG, "the whole clip is read");
                }
            }
            if (!sample) continue;
            if (index == video_index_) {
                if (!push_video(sample.Get(), time, want_header)) continue;
                want_header = false;
            } else if (index == audio_index_) {
                push_audio(sample.Get());
            }
        }
    }

    bool push_video(IMFSample* sample, LONGLONG time, bool want_header) {
        ComPtr<IMFMediaBuffer> buffer;
        if (FAILED(sample->ConvertToContiguousBuffer(&buffer))) return false;
        BYTE* data = nullptr;
        DWORD length = 0;
        if (FAILED(buffer->Lock(&data, nullptr, &length))) return false;
        DemuxedFrame f;
        f.annex_b.assign(data, data + length);
        buffer->Unlock();
        f.annex_b = to_annex_b(std::move(f.annex_b));
        f.keyframe = MFGetAttributeUINT32(sample, MFSampleExtension_CleanPoint, FALSE) != 0;
        f.pts_us = time / 10;
        // The parameter sets before the first key frame of a run, in case the
        // file kept them only in the track header.
        if (want_header && f.keyframe && !sequence_header_.empty()) {
            f.annex_b.insert(f.annex_b.begin(), sequence_header_.begin(), sequence_header_.end());
        }
        const auto cap = [this] {
            const bool starving = audio_index_ != kNone && audio_->buffered_frames() < kResumeFrames &&
                                  !audio_->ended();
            return starving ? kFrameCapStarving : kFrameCap;
        };
        return source_->push(std::move(f), cap, [this] { return cancelled(); });
    }

    void push_audio(IMFSample* sample) {
        ComPtr<IMFMediaBuffer> buffer;
        if (FAILED(sample->ConvertToContiguousBuffer(&buffer))) return;
        BYTE* data = nullptr;
        DWORD length = 0;
        if (FAILED(buffer->Lock(&data, nullptr, &length))) return;
        const auto* pcm = reinterpret_cast<const int16_t*>(data);
        const size_t frames = length / (sizeof(int16_t) * audio::ClipAudioStream::kChannels);
        size_t done = 0;
        while (done < frames && !cancelled()) {
            done += audio_->write(pcm + done * audio::ClipAudioStream::kChannels, frames - done);
            if (done < frames) std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
        buffer->Unlock();
    }

    void do_seek(int64_t target_us) {
        PROPVARIANT var;
        PropVariantInit(&var);
        var.vt = VT_I8;
        var.hVal.QuadPart = target_us * 10;  // 100 ns units
        const HRESULT hr = reader_->SetCurrentPosition(GUID_NULL, var);
        PropVariantClear(&var);
        if (FAILED(hr)) {
            MELLO_LOG_WARN(TAG, "seek to %.2f s failed: hr=0x%08X", target_us / 1e6, static_cast<unsigned>(hr));
            return;
        }
        // The old frames go first, then the ring: no frame from before the
        // seek can land in the ring after the flush.
        source_->clear();
        {
            std::lock_guard<std::mutex> lock(viewer_mutex_);
            if (viewer_) viewer_->flush();
        }
        const uint64_t frames = static_cast<uint64_t>(target_us) * audio::ClipAudioStream::kSampleRate / 1'000'000;
        audio_->reset(frames + static_cast<uint64_t>(latency_us_) * audio::ClipAudioStream::kSampleRate / 1'000'000);
        demux_ended_.store(false);
        std::lock_guard<std::mutex> lock(clock_mutex_);
        wall_pos_ = target_us;
        wall_at_ = now_us();
        audio_done_ = false;
        started_ = false;
        buffering_ = has_audio_;
        apply_audio_pause_locked();
        MELLO_LOG_INFO(TAG, "seek to %.2f s", target_us / 1e6);
    }

    static constexpr DWORD kNone = 0xFFFFFFFF;

    ClipPlayerOptions options_;
    std::shared_ptr<audio::ClipAudioStream> audio_;
    std::shared_ptr<ClipFrameSource> source_;
    std::shared_ptr<std::atomic<bool>> stop_flag_ = std::make_shared<std::atomic<bool>>(false);
    std::shared_ptr<std::atomic<bool>> waiting_ = std::make_shared<std::atomic<bool>>(false);
    std::atomic<bool> stopped_{false};
    std::atomic<bool> failed_{false};
    std::atomic<bool> demux_ended_{false};
    std::atomic<bool> seek_pending_{false};
    std::atomic<int64_t> seek_to_{0};
    std::atomic<int64_t> duration_us_{0};
    std::atomic<uint32_t> width_{0};
    std::atomic<uint32_t> height_{0};

    // Demux thread.
    ComPtr<ClipByteStream> byte_stream_;
    ComPtr<IMFMediaSource> media_source_;
    ComPtr<IMFSourceReader> reader_;
    DWORD video_index_ = kNone;
    DWORD audio_index_ = kNone;
    std::vector<uint8_t> sequence_header_;

    std::mutex viewer_mutex_;
    std::unique_ptr<ViewerPipeline> viewer_;
    ViewerPipeline::NativeFrameCallback native_cb_;

    mutable std::mutex clock_mutex_;
    bool has_audio_ = false;
    bool audio_done_ = false;  // the sound ended: the wall clock runs on
    bool paused_ = false;
    bool buffering_ = false;
    bool started_ = false;
    int64_t latency_us_ = 0;
    int64_t wall_pos_ = 0;
    int64_t wall_at_ = 0;

    std::thread thread_;
};

}  // namespace

std::unique_ptr<ClipPlayer> ClipPlayer::start(ClipPlayerOptions options) {
    if (!options.read || options.length == 0) return nullptr;
    return std::make_unique<WinClipPlayer>(std::move(options));
}

}  // namespace mello::clip
