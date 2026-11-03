#ifdef _WIN32
#include "replay_session.hpp"
#include "replay_audio_mix.hpp"
#include "replay_mux.hpp"
#include "../audio/audio_pipeline.hpp"
#include "../audio/shared_game_audio.hpp"
#include "../util/log.hpp"
#include "../video/encoder_factory.hpp"
#include "../video/video_pipeline.hpp"
#include "../video/video_preprocessor.hpp"
#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstring>
#include <vector>

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/session";
constexpr auto kPosterWait = std::chrono::milliseconds(300);

uint64_t now_us() {
    return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::microseconds>(
                                     std::chrono::steady_clock::now().time_since_epoch())
                                     .count());
}

void set_error(std::string* error, const char* text) {
    MELLO_LOG_WARN(TAG, "start: %s", text);
    if (error) *error = text;
}

// A session whose capture thread did not stop: it stays allocated for good,
// because that thread can still call into it.
void keep_forever(std::shared_ptr<ReplaySession> session) {
    static auto* abandoned = new std::vector<std::shared_ptr<ReplaySession>>();
    abandoned->push_back(std::move(session));
}

}  // namespace

ReplaySession::ReplaySession(Key) {}

std::shared_ptr<ReplaySession> ReplaySession::start(const ReplayConfig& config, std::string* error) {
    auto session = std::make_shared<ReplaySession>(Key{});
    if (!session->start_impl(config, error)) {
        session->stop();
        return nullptr;
    }
    return session;
}

bool ReplaySession::start_impl(const ReplayConfig& config, std::string* error) {
    config_ = config;
    if (config_.fps == 0) config_.fps = 60;

    // The game that this replay records, to match a stream of the same game.
    if (config_.source) {
        game_key_ = config_.source->mode == video::CaptureMode::Reference ? kReferenceKey
                                                                          : config_.pid;
    } else {
        if (!config_.hwnd) {
            set_error(error, "no game window");
            return false;
        }
        DWORD pid = 0;
        GetWindowThreadProcessId(static_cast<HWND>(config_.hwnd), &pid);
        game_key_ = config_.pid != 0 ? config_.pid : static_cast<uint32_t>(pid);
    }

    std::shared_ptr<video::CaptureManager> own = make_own_capture(error);
    if (!own) return false;

    // The encode size: the clip quality's height, the source aspect. It stays
    // fixed for the session (a clip has one picture size).
    const uint32_t cap_w = own->width() & ~1u;
    const uint32_t cap_h = own->height() & ~1u;
    const auto size = video::VideoPipeline::encode_size_for(cap_w, cap_h, config_.height, config_.fps);
    enc_w_ = size.width;
    enc_h_ = size.height;
    if (enc_w_ < video::kMinEncodeWidth || enc_h_ < video::kMinEncodeHeight) {
        set_error(error, "the game window is too small (minimized?)");
        return false;
    }

    // The rings.
    ReplayVideoRing::Config vring;
    vring.dir = config_.dir;
    vring.segment_bytes = ReplayVideoRing::segment_bytes_for(config_.bitrate_kbps);
    ReplayAudioRing::Config aring;
    aring.dir = config_.dir;
    if (!video_ring_.start(vring) || !audio_ring_.start(aring)) {
        set_error(error, "cannot make the ring files");
        return false;
    }

    // The game audio: process loopback of the game's process tree, the one
    // capture of the game that a stream shares (F7). Never endpoint loopback:
    // it records the crew voice and every other app twice (CLIPS.md 15.12).
    if (config_.pid != 0) {
        game_audio_ = config_.game_audio && config_.game_audio->pid() == config_.pid
                          ? config_.game_audio
                          : audio::SharedGameAudio::open(config_.pid);
        if (game_audio_) {
            game_audio_consumer_ =
                game_audio_->add([this](const int16_t* s, size_t n) { on_game_audio(s, n); });
            has_game_audio_ = game_audio_consumer_ != 0;
        } else {
            MELLO_LOG_WARN(TAG, "no process loopback for pid %u: clips have no game audio",
                           config_.pid);
        }
    }

    std::lock_guard<std::mutex> control(control_mutex_);
    if (!open_capture(std::move(own), true, config_.fps, error)) return false;
    MELLO_LOG_INFO(TAG, "recording %u: %ux%u at %u fps, %u kbps, game audio %s", game_key_, enc_w_,
                   enc_h_, config_.fps, config_.bitrate_kbps, has_game_audio_ ? "on" : "off");
    return true;
}

std::shared_ptr<video::CaptureManager> ReplaySession::make_own_capture(std::string* error) const {
    // WGC on the game window, and nothing else. No resolve_capture_target: it
    // would turn a window that cannot carry a stream into its process and
    // run the whole ladder, monitor capture included (CLIPS.md 15.3).
    video::CaptureSourceDesc desc{};
    if (config_.source) {
        desc = *config_.source;
    } else {
        desc.mode = video::CaptureMode::Window;
        desc.hwnd = config_.hwnd;
    }
    video::GraphicsDevice device = video::create_d3d11_device();
    if (!device.handle) {
        set_error(error, "no graphics device");
        return nullptr;
    }
    auto manager = video::CaptureManager::create_from(device, video::create_capture_source(desc), desc);
    if (!manager) set_error(error, "the game window cannot be captured");
    return manager;
}

bool ReplaySession::open_capture(std::shared_ptr<video::CaptureManager> manager, bool own,
                                 uint32_t fps, std::string* error) {
    // The preprocessor and the encoder live on the manager's device: the
    // capture's textures are on it (zero copy, 14-VIDEO-PIPELINE.md 2.2).
    preprocessor_ = std::make_unique<video::VideoPreprocessor>();
    if (!preprocessor_->initialize(manager->device(), manager->width(), manager->height(), enc_w_,
                                   enc_h_)) {
        set_error(error, "no video processor");
        preprocessor_.reset();
        return false;
    }

    // The recording encoder: a 1 s GOP, the quality tuning (14-VIDEO-PIPELINE.md 6.8).
    video::EncoderConfig enc{};
    enc.width = enc_w_;
    enc.height = enc_h_;
    enc.fps = fps;
    enc.bitrate_kbps = config_.bitrate_kbps;
    enc.keyframe_interval = fps;
    enc.intent = video::EncoderIntent::Recording;
    encoder_ = video::create_best_encoder(manager->device(), enc);
    if (!encoder_) {
        set_error(error, "no hardware encoder");
        preprocessor_.reset();
        return false;
    }
    encode_fps_ = fps;
    // The encode thread's own state, before it starts.
    applied_fps_ = fps;
    frames_since_key_ = 0;
    encode_ms_mean_ = 0.0;
    window_offered_ = frames_offered_.load(std::memory_order_relaxed);
    window_drops_ = queue_drops_.load(std::memory_order_relaxed);

    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        queue_head_ = 0;
        queue_count_ = 0;
    }
    next_deadline_us_ = 0;
    monitor_frames_.store(false, std::memory_order_relaxed);
    keyframe_wanted_.store(true, std::memory_order_relaxed);
    running_.store(true, std::memory_order_release);
    encode_thread_ = std::thread([this] { encode_loop(); });

    // Atomic: state() reads it without control_mutex_.
    std::atomic_store(&manager_, std::move(manager));
    handled_swap_generation_ = manager_->swap_generation();
    consumer_id_ = manager_->add_consumer([this](const video::CapturedFrame& f) { on_frame(f); });
    // The replay lane of the stream gate: the reference source gives the game
    // audio of its own picture, at its capture times. The ring takes it in
    // place of a process loopback.
    if (consumer_id_ != 0 && own && config_.source && config_.pid == 0 &&
        manager_->provides_timed_audio()) {
        manager_->set_timed_audio_callback(
            consumer_id_, [this](const float* samples, uint32_t frames, uint32_t channels,
                                 uint32_t rate, int64_t capture_us) {
                on_timed_audio(samples, frames, channels, rate, capture_us);
            });
        has_game_audio_ = true;
    }
    if (consumer_id_ == 0 || (own && !manager_->start(fps))) {
        set_error(error, "the capture did not start");
        close_capture();
        return false;
    }
    MELLO_LOG_INFO(TAG, "capture: %s %s %ux%u -> %ux%u at %u fps, encoder %s",
                   own ? "own" : "the stream's", manager_->backend_name(), manager_->width(),
                   manager_->height(), enc_w_, enc_h_, fps, encoder_->name());
    return true;
}

bool ReplaySession::close_capture() {
    if (manager_ && consumer_id_ != 0) {
        const auto result = manager_->remove_consumer(consumer_id_);
        consumer_id_ = 0;
        if (result == video::CaptureManager::RemoveResult::Abandoned) {
            // A capture thread is stuck in the driver or in on_frame and may
            // still use the session: it stays allocated for good.
            MELLO_LOG_ERROR(TAG, "the capture did not stop; the replay session is abandoned");
            failed_.store(true, std::memory_order_relaxed);
            if (auto self = weak_from_this().lock()) keep_forever(std::move(self));
            return false;
        }
    }
    // No frame comes in now: the encode thread can stop, and the NV12 slots
    // in its queue go with the preprocessor.
    running_.store(false, std::memory_order_release);
    queue_cv_.notify_all();
    if (encode_thread_.joinable()) encode_thread_.join();
    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        queue_count_ = 0;
    }
    if (encoder_) {
        encoder_->shutdown();
        encoder_.reset();
    }
    preprocessor_.reset();
    {
        std::lock_guard<std::mutex> lock(poster_mutex_);
        poster_staging_.Reset();
        poster_ready_ = false;
    }
    std::atomic_store(&manager_, std::shared_ptr<video::CaptureManager>());
    return true;
}

void ReplaySession::follow_stream(const std::shared_ptr<video::CaptureManager>& manager,
                                  uint32_t stream_key, uint32_t stream_fps) {
    if (!manager || stream_key == 0 || stream_key != game_key_) return;
    std::lock_guard<std::mutex> control(control_mutex_);
    if (stopped_.load(std::memory_order_acquire) || failed_.load(std::memory_order_relaxed) ||
        following_.load(std::memory_order_acquire)) {
        return;
    }
    // The stream started exactly as it would without the replay (F2). The
    // replay's own capture stops; the game keeps one capture.
    if (!close_capture()) return;
    // F1: the stream's rate, never above it.
    const uint32_t fps = std::min(config_.fps, stream_fps > 0 ? stream_fps : config_.fps);
    std::string error;
    if (!open_capture(manager, false, fps, &error)) {
        // F5: the replay is off; the stream does not notice.
        MELLO_LOG_ERROR(TAG, "follow the stream: %s; the replay is off", error.c_str());
        failed_.store(true, std::memory_order_relaxed);
        return;
    }
    following_.store(true, std::memory_order_release);
}

void ReplaySession::leave_stream() {
    std::lock_guard<std::mutex> control(control_mutex_);
    if (!following_.load(std::memory_order_acquire)) return;
    following_.store(false, std::memory_order_release);
    if (!close_capture()) return;
    if (stopped_.load(std::memory_order_acquire)) return;
    // Back to WGC window. The stream's capture, the hook too, stops with the
    // stream (14-VIDEO-PIPELINE.md 4.8).
    std::string error;
    std::shared_ptr<video::CaptureManager> own = make_own_capture(&error);
    if (!own || !open_capture(std::move(own), true, config_.fps, &error)) {
        MELLO_LOG_ERROR(TAG, "back to the own capture: %s; the replay is off", error.c_str());
        failed_.store(true, std::memory_order_relaxed);
    }
}

ReplaySession::~ReplaySession() {
    stop();
}

void ReplaySession::stop() {
    std::lock_guard<std::mutex> control(control_mutex_);
    if (stopped_.exchange(true, std::memory_order_acq_rel)) return;
    if (game_audio_) {
        // Waits for a call that runs now. A stream of the game keeps it.
        if (game_audio_consumer_ != 0) game_audio_->remove(game_audio_consumer_);
        game_audio_consumer_ = 0;
        game_audio_.reset();
    }
    following_.store(false, std::memory_order_release);
    if (!close_capture()) return;  // abandoned: everything stays allocated
    video_ring_.stop();
    audio_ring_.stop();
}

ReplayState ReplaySession::state() const {
    if (failed_.load(std::memory_order_relaxed) || !video_ring_.is_active()) return ReplayState::Failed;
    // The pointer changes only under control_mutex_, with atomic stores; a
    // copy here may be one switch old, which a state poll tolerates.
    const std::shared_ptr<video::CaptureManager> manager = std::atomic_load(&manager_);
    if (!manager) return ReplayState::Failed;
    if (load_.level() == ReplayLoad::Level::Paused) return ReplayState::Paused;
    if (monitor_frames_.load(std::memory_order_relaxed)) return ReplayState::Waiting;
    switch (manager->state()) {
        case video::CaptureState::Failed:
            return ReplayState::Failed;
        case video::CaptureState::Capturing:
            return ReplayState::Recording;
        default:
            return ReplayState::Waiting;
    }
}

void ReplaySession::on_frame(const video::CapturedFrame& frame) {
    if (!running_.load(std::memory_order_acquire)) return;
    frames_captured_.fetch_add(1, std::memory_order_relaxed);

    // A stream's ladder on a monitor method: the frames show the desktop.
    // The replay waits for a method that shows only the game (3.3, F2).
    if (!frame.target_only) {
        monitor_frames_.store(true, std::memory_order_relaxed);
        return;
    }
    monitor_frames_.store(false, std::memory_order_relaxed);

    // The poster takes the first frame after the press, whatever the rate.
    if (poster_wanted_.exchange(false, std::memory_order_acq_rel)) copy_poster_frame(frame.texture);

    // F3: after 60 s with no overload, one level back up. Out of a pause the
    // next frame starts a new segment.
    const ReplayLoad::Level before = load_.level();
    if (load_.recover(now_us())) {
        MELLO_LOG_INFO(TAG, "load: 60 s with no overload, now %s",
                       load_.level() == ReplayLoad::Level::Full ? "full rate" : "30 fps");
        if (before == ReplayLoad::Level::Paused) keyframe_wanted_.store(true, std::memory_order_relaxed);
    }
    const uint32_t fps = load_.fps(encode_fps_);
    if (fps == 0) return;  // paused: the clip shows the last frame for the gap
    if (!video::VideoPipeline::decimation_accepts(frame.capture_us, next_deadline_us_, fps)) {
        return;
    }
    // A new capture method starts a new segment.
    if (frame.swap_generation != handled_swap_generation_) {
        handled_swap_generation_ = frame.swap_generation;
        keyframe_wanted_.store(true, std::memory_order_relaxed);
    }

    const auto result = preprocessor_->convert(frame.texture, nullptr);
    if (!result.texture) return;

    {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        if (queue_count_ == kQueueCap) {
            // Newest wins. The dropped frame was never encoded, so no later
            // frame refers to it: the clip shows the frame before it longer.
            queue_head_ = (queue_head_ + 1) % kQueueCap;
            --queue_count_;
            queue_drops_.fetch_add(1, std::memory_order_relaxed);
        }
        queue_[(queue_head_ + queue_count_) % kQueueCap] = Job{result.texture, frame.capture_us};
        ++queue_count_;
    }
    frames_offered_.fetch_add(1, std::memory_order_relaxed);
    queue_cv_.notify_one();
}

bool ReplaySession::shed_for_stream() {
    const bool shed = load_.shed(now_us(), encode_fps_);
    if (shed) {
        MELLO_LOG_WARN(TAG, "load: the stream is overloaded, the replay sheds first: now %s",
                       load_.level() == ReplayLoad::Level::Paused ? "paused" : "30 fps");
    }
    return shed;
}

void ReplaySession::encode_loop() {
    for (;;) {
        Job job;
        {
            std::unique_lock<std::mutex> lock(queue_mutex_);
            queue_cv_.wait(lock, [this] {
                return queue_count_ > 0 || !running_.load(std::memory_order_acquire);
            });
            if (!running_.load(std::memory_order_acquire)) return;
            job = queue_[queue_head_];
            queue_head_ = (queue_head_ + 1) % kQueueCap;
            --queue_count_;
        }
        // F3: the rate of the load level. The rate control follows it.
        const uint32_t wanted = load_.fps(encode_fps_);
        if (wanted != 0 && wanted != applied_fps_) {
            encoder_->set_framerate(wanted);
            applied_fps_ = wanted;
        }
        // A 1 s GOP at any rate: the encoder's GOP is in frames of the full
        // rate, so at 30 fps an IDR comes every 30 frames from here.
        if (keyframe_wanted_.exchange(false, std::memory_order_relaxed) ||
            video_ring_.take_keyframe_request() || frames_since_key_ >= applied_fps_) {
            encoder_->request_keyframe();
        }
        const auto t0 = std::chrono::steady_clock::now();
        video::EncodedPacket packet{};
        if (!encoder_->encode(job.texture, packet)) {
            // F5: a replay fault turns the replay off. It never touches the
            // capture that a stream may share.
            MELLO_LOG_ERROR(TAG, "encode failed: the replay is off");
            failed_.store(true, std::memory_order_relaxed);
            return;
        }
        const double encode_ms =
            std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count();
        // About 1 s of history, as the stream's mean (video_pipeline.cpp).
        encode_ms_mean_ += (encode_ms - encode_ms_mean_) / 60.0;
        if (packet.data.empty()) continue;
        frames_since_key_ = packet.is_keyframe ? 1 : frames_since_key_ + 1;
        const uint64_t encoded = frames_encoded_.fetch_add(1, std::memory_order_relaxed) + 1;
        video_ring_.push(std::move(packet.data), job.capture_us, packet.is_keyframe);
        if (encoded % 60 == 0) check_own_load();
    }
}

void ReplaySession::check_own_load() {
    // The replay's own encoder cannot hold its rate: the same rule as the
    // stream's (VideoPipeline::encoder_is_overloaded). The encoder gives up
    // its quality features first, then the replay sheds a rate step
    // (14-VIDEO-PIPELINE.md 6.8: without a stream, the same overload signal).
    const uint64_t offered = frames_offered_.load(std::memory_order_relaxed);
    const uint64_t drops = queue_drops_.load(std::memory_order_relaxed);
    video::VideoPipeline::EncoderLoadSample sample{};
    sample.frames_captured = offered - window_offered_;
    sample.queue_drops = drops - window_drops_;
    sample.mean_encode_ms = encode_ms_mean_;
    sample.target_fps = applied_fps_;
    window_offered_ = offered;
    window_drops_ = drops;
    if (!video::VideoPipeline::encoder_is_overloaded(sample)) return;
    if (encoder_->cost_tier() < encoder_->max_cost_tier() && encoder_->reduce_cost_tier()) {
        MELLO_LOG_WARN(TAG, "load: the replay encoder is overloaded, cost tier %d",
                       encoder_->cost_tier());
        return;
    }
    if (load_.shed(now_us(), encode_fps_)) {
        MELLO_LOG_WARN(TAG, "load: the replay encoder is overloaded, now %s",
                       load_.level() == ReplayLoad::Level::Paused ? "paused" : "30 fps");
    }
}

void ReplaySession::on_game_audio(const int16_t* samples, size_t count) {
    // 48 kHz stereo. The time of the first frame: the arrival less the length
    // of the packet, as the stream's game audio has it (CLIPS.md 15.4).
    const size_t frames = count / ReplayAudioRing::kChannels;
    if (frames == 0) return;
    const uint64_t capture_us = now_us() - frames * 1'000'000 / ReplayAudioRing::kRate;
    audio_ring_.push(samples, frames, capture_us);
}

void ReplaySession::on_timed_audio(const float* samples, uint32_t frames, uint32_t channels,
                                   uint32_t rate, int64_t capture_us) {
    // The reference clip's audio is 48 kHz stereo, as the ring keeps it.
    if (rate != ReplayAudioRing::kRate || channels != ReplayAudioRing::kChannels || frames == 0) {
        return;
    }
    timed_pcm_.resize(static_cast<size_t>(frames) * channels);
    for (size_t i = 0; i < timed_pcm_.size(); ++i) {
        const float v = std::clamp(samples[i], -1.0f, 1.0f);
        timed_pcm_[i] = static_cast<int16_t>(std::lround(v * 32767.0f));
    }
    audio_ring_.push(timed_pcm_.data(), frames, static_cast<uint64_t>(capture_us));
}

bool ReplaySession::reference_info(video::ReferenceInfo& out) const {
    const auto manager = std::atomic_load(&manager_);
    auto* ref = manager ? dynamic_cast<video::ReferenceCapture*>(manager->source()) : nullptr;
    if (!ref) return false;
    out = ref->info();
    return true;
}

void ReplaySession::copy_poster_frame(ID3D11Texture2D* texture) {
    if (!texture) return;
    D3D11_TEXTURE2D_DESC desc{};
    texture->GetDesc(&desc);
    if (desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM && desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB) {
        return;  // HDR or another format: the poster comes from the MP4
    }
    Microsoft::WRL::ComPtr<ID3D11Device> device;
    texture->GetDevice(&device);
    D3D11_TEXTURE2D_DESC staging_desc{};
    staging_desc.Width = desc.Width;
    staging_desc.Height = desc.Height;
    staging_desc.MipLevels = 1;
    staging_desc.ArraySize = 1;
    staging_desc.Format = desc.Format;
    staging_desc.SampleDesc.Count = 1;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
    if (FAILED(device->CreateTexture2D(&staging_desc, nullptr, &staging))) return;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
    device->GetImmediateContext(&context);
    // A submit, no wait (12-STREAMING.md 3.7, F6). The map on the capture's caller waits.
    context->CopyResource(staging.Get(), texture);
    {
        std::lock_guard<std::mutex> lock(poster_mutex_);
        poster_staging_ = staging;
        poster_ready_ = true;
    }
    poster_cv_.notify_all();
}

bool ReplaySession::write_live_poster(const std::filesystem::path& poster,
                                      std::chrono::milliseconds wait) {
    Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
    {
        std::unique_lock<std::mutex> lock(poster_mutex_);
        if (!poster_cv_.wait_for(lock, wait, [this] { return poster_ready_; })) return false;
        staging = poster_staging_;
        poster_staging_.Reset();
        poster_ready_ = false;
    }
    if (!staging) return false;
    D3D11_TEXTURE2D_DESC desc{};
    staging->GetDesc(&desc);
    Microsoft::WRL::ComPtr<ID3D11Device> device;
    staging->GetDevice(&device);
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
    device->GetImmediateContext(&context);
    D3D11_MAPPED_SUBRESOURCE mapped{};
    if (FAILED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) return false;
    const bool ok = write_poster_jpeg(static_cast<const uint8_t*>(mapped.pData), desc.Width,
                                      desc.Height, mapped.RowPitch, poster);
    context->Unmap(staging.Get(), 0);
    return ok;
}

bool ReplaySession::capture(float seconds, audio::AudioPipeline* voice,
                            const std::filesystem::path& mp4, const std::filesystem::path& poster,
                            ReplayClip* out) {
    if (seconds <= 0.0f) return false;
    const uint64_t end_us = now_us();
    const auto want_us = static_cast<uint64_t>(static_cast<double>(seconds) * 1e6);
    const uint64_t start_us = end_us > want_us ? end_us - want_us : 0;

    ReplayVideoRing::Window video;
    ReplayAudioRing::Window game;
    {
        std::lock_guard<std::mutex> control(control_mutex_);
        if (!manager_ || !running_.load(std::memory_order_acquire)) {
            MELLO_LOG_WARN(TAG, "capture: the replay is not running");
            return false;
        }
        {
            std::lock_guard<std::mutex> lock(poster_mutex_);
            poster_ready_ = false;
            poster_staging_.Reset();
        }
        poster_wanted_.store(true, std::memory_order_release);
        video_ring_.flush();
        audio_ring_.flush();
        // 1. T0 is the last keyframe at or before the start. Pin to the end.
        video = video_ring_.pin_window(start_us, end_us);
        if (video.empty()) {
            MELLO_LOG_WARN(TAG, "capture: no video in the ring");
            return false;
        }
        game = audio_ring_.window(video.units().front().capture_us, end_us);
    }
    const uint64_t t0 = video.units().front().capture_us;

    // 2. The audio sources on the clip timeline.
    std::vector<ClipAudioSource> sources;
    {
        ClipAudioSource source;
        source.channels = 2;
        std::vector<int16_t> pcm(ReplayAudioRing::kChunkFrames * ReplayAudioRing::kChannels);
        for (const AudioChunkRef& chunk : game.chunks()) {
            // A chunk the writer reused is a gap: silence at its place.
            if (!game.read(chunk, pcm.data())) continue;
            TimedPcm t;
            t.capture_us = chunk.capture_us;
            t.samples.assign(pcm.begin(), pcm.begin() + chunk.frames * ReplayAudioRing::kChannels);
            source.chunks.push_back(std::move(t));
        }
        if (!source.chunks.empty()) sources.push_back(std::move(source));
    }
    if (voice) {
        audio::ClipBuffer::VoiceWindow window;
        if (voice->clip_voice_window(t0, end_us, window)) {
            ClipAudioSource crew;
            crew.channels = 1;
            crew.chunks = std::move(window.crew);
            crew.gain_points = std::move(window.crew_volume);  // CLIPS.md 15.4
            ClipAudioSource mic;
            mic.channels = 1;
            mic.chunks = std::move(window.mic);
            sources.push_back(std::move(crew));
            sources.push_back(std::move(mic));
        }
    }

    // 3. The mux: video passthrough, the audio mix as AAC.
    ClipMuxInput in;
    in.units = video.units();
    in.read_unit = [&video](const AccessUnitRef& u, std::vector<uint8_t>& bytes) {
        return video.read(u, bytes);
    };
    in.width = enc_w_;
    in.height = enc_h_;
    in.fps = config_.fps;
    in.end_us = end_us;
    in.audio = mix_clip_audio(t0, end_us, sources);
    ClipMuxResult mux{};
    const auto t_mux = std::chrono::steady_clock::now();
    const bool ok = write_clip_mp4(in, mp4, &mux);
    // 4. Unpin.
    uint64_t last_idr_us = 0;
    for (const auto& u : video.units()) {
        if (u.keyframe) last_idr_us = u.capture_us - t0;
    }
    video.release();
    if (!ok) return false;

    // 5. The poster: the frame after the press, else the last IDR of the clip.
    bool live = write_live_poster(poster, kPosterWait);
    if (!live && !write_poster_from_mp4(mp4, last_idr_us, poster)) {
        MELLO_LOG_WARN(TAG, "capture: no poster");
    }
    poster_wanted_.store(false, std::memory_order_relaxed);

    const double mux_s =
        std::chrono::duration<double>(std::chrono::steady_clock::now() - t_mux).count();
    MELLO_LOG_INFO(TAG, "capture: %.2f s clip, %ux%u, %llu bytes, mux %.2f s, poster %s",
                   static_cast<double>(mux.duration_us) / 1e6, enc_w_, enc_h_,
                   static_cast<unsigned long long>(mux.bytes), mux_s, live ? "live" : "from the MP4");
    if (out) {
        out->duration_us = mux.duration_us;
        out->width = enc_w_;
        out->height = enc_h_;
        out->bytes = mux.bytes;
        out->has_game_audio = has_game_audio_;
        out->poster_live = live;
    }
    return true;
}

ReplaySession::Stats ReplaySession::stats() const {
    Stats s;
    s.frames_captured = frames_captured_.load(std::memory_order_relaxed);
    s.frames_offered = frames_offered_.load(std::memory_order_relaxed);
    s.frames_encoded = frames_encoded_.load(std::memory_order_relaxed);
    s.encode_queue_drops = queue_drops_.load(std::memory_order_relaxed);
    s.video_ring = video_ring_.stats();
    s.audio_ring = audio_ring_.stats();
    return s;
}

}  // namespace mello::replay
#endif
