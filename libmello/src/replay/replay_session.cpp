#ifdef _WIN32
#include "replay_session.hpp"
#include "replay_audio_mix.hpp"
#include "replay_mux.hpp"
#include "../audio/audio_pipeline.hpp"
#include "../audio/capture_wasapi_loopback.hpp"
#include "../util/log.hpp"
#include "../video/encoder_factory.hpp"
#include "../video/video_pipeline.hpp"
#include "../video/video_preprocessor.hpp"
#include <algorithm>
#include <chrono>
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

    // 1. The capture: the game window with WGC, and nothing else. No
    // resolve_capture_target here: it would turn a window that cannot carry
    // a stream into its process and run the whole ladder, monitor capture
    // included (plan 3.3).
    video::CaptureSourceDesc desc{};
    if (config_.source) {
        desc = *config_.source;
    } else {
        if (!config_.hwnd) {
            set_error(error, "no game window");
            return false;
        }
        desc.mode = video::CaptureMode::Window;
        desc.hwnd = config_.hwnd;
    }
    video::GraphicsDevice device = video::create_d3d11_device();
    if (!device.handle) {
        set_error(error, "no graphics device");
        return false;
    }
    manager_ = video::CaptureManager::create_from(device, video::create_capture_source(desc), desc);
    if (!manager_) {
        set_error(error, "the game window cannot be captured");
        return false;
    }

    // 2. The encode size: the clip quality's height, the source aspect.
    const uint32_t cap_w = manager_->width() & ~1u;
    const uint32_t cap_h = manager_->height() & ~1u;
    const auto size = video::VideoPipeline::encode_size_for(cap_w, cap_h, config_.height, config_.fps);
    enc_w_ = size.width;
    enc_h_ = size.height;
    if (enc_w_ < video::kMinEncodeWidth || enc_h_ < video::kMinEncodeHeight) {
        set_error(error, "the game window is too small (minimized?)");
        return false;
    }

    preprocessor_ = std::make_unique<video::VideoPreprocessor>();
    if (!preprocessor_->initialize(manager_->device(), manager_->width(), manager_->height(), enc_w_,
                                   enc_h_)) {
        set_error(error, "no video processor");
        return false;
    }

    // 3. The recording encoder: a 1 s GOP, the quality tuning (plan 3.4).
    video::EncoderConfig enc{};
    enc.width = enc_w_;
    enc.height = enc_h_;
    enc.fps = config_.fps;
    enc.bitrate_kbps = config_.bitrate_kbps;
    enc.keyframe_interval = config_.fps;
    enc.intent = video::EncoderIntent::Recording;
    encoder_ = video::create_best_encoder(manager_->device(), enc);
    if (!encoder_) {
        set_error(error, "no hardware encoder");
        return false;
    }

    // 4. The rings.
    ReplayVideoRing::Config vring;
    vring.dir = config_.dir;
    vring.segment_bytes = ReplayVideoRing::segment_bytes_for(config_.bitrate_kbps);
    ReplayAudioRing::Config aring;
    aring.dir = config_.dir;
    if (!video_ring_.start(vring) || !audio_ring_.start(aring)) {
        set_error(error, "cannot make the ring files");
        return false;
    }

    // 5. The game audio: process loopback of the game's process tree. Never
    // endpoint loopback: it records the crew voice and every other app twice
    // (plan 9.1).
    if (config_.pid != 0) {
        loopback_ = std::make_unique<audio::WasapiLoopbackCapture>();
        if (loopback_->initialize_scoped(audio::LoopbackScope::IncludeProcessTree, config_.pid) &&
            loopback_->scope() == audio::LoopbackScope::IncludeProcessTree &&
            loopback_->start([this](const int16_t* s, size_t n) { on_game_audio(s, n); })) {
            has_game_audio_ = true;
        } else {
            MELLO_LOG_WARN(TAG, "no process loopback for pid %u: clips have no game audio",
                           config_.pid);
            loopback_.reset();
        }
    }

    // 6. The encode thread, then the capture.
    running_.store(true, std::memory_order_release);
    encode_thread_ = std::thread([this] { encode_loop(); });
    handled_swap_generation_ = manager_->swap_generation();
    keyframe_wanted_.store(true, std::memory_order_relaxed);
    consumer_id_ = manager_->add_consumer([this](const video::CapturedFrame& f) { on_frame(f); });
    if (consumer_id_ == 0 || !manager_->start(config_.fps)) {
        set_error(error, "the capture did not start");
        return false;
    }

    MELLO_LOG_INFO(TAG, "recording: %s %ux%u -> %ux%u at %u fps, %u kbps, encoder %s, game audio %s",
                   manager_->backend_name(), manager_->width(), manager_->height(), enc_w_, enc_h_,
                   config_.fps, config_.bitrate_kbps, encoder_->name(),
                   has_game_audio_ ? "on" : "off");
    return true;
}

ReplaySession::~ReplaySession() {
    stop();
}

void ReplaySession::stop() {
    std::lock_guard<std::mutex> control(control_mutex_);
    if (loopback_) {
        loopback_->stop();
        loopback_.reset();
    }
    if (manager_ && consumer_id_ != 0) {
        const auto result = manager_->remove_consumer(consumer_id_);
        consumer_id_ = 0;
        if (result == video::CaptureManager::RemoveResult::Abandoned) {
            // A capture thread is stuck in the driver and may still call
            // on_frame: the session and all it holds stay allocated.
            MELLO_LOG_ERROR(TAG, "the capture did not stop; the replay session is abandoned");
            failed_.store(true, std::memory_order_relaxed);
            if (auto self = weak_from_this().lock()) keep_forever(std::move(self));
            return;
        }
    }
    running_.store(false, std::memory_order_release);
    queue_cv_.notify_all();
    if (encode_thread_.joinable()) encode_thread_.join();
    if (encoder_) {
        encoder_->shutdown();
        encoder_.reset();
    }
    preprocessor_.reset();
    video_ring_.stop();
    audio_ring_.stop();
    {
        std::lock_guard<std::mutex> lock(poster_mutex_);
        poster_staging_.Reset();
    }
    manager_.reset();
}

ReplayState ReplaySession::state() const {
    if (failed_.load(std::memory_order_relaxed) || !video_ring_.is_active()) return ReplayState::Failed;
    if (!manager_) return ReplayState::Failed;
    switch (manager_->state()) {
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

    // The poster takes the first frame after the press, whatever the rate.
    if (poster_wanted_.exchange(false, std::memory_order_acq_rel)) copy_poster_frame(frame.texture);

    if (!video::VideoPipeline::decimation_accepts(frame.capture_us, next_deadline_us_, config_.fps)) {
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
    queue_cv_.notify_one();
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
        if (keyframe_wanted_.exchange(false, std::memory_order_relaxed) ||
            video_ring_.take_keyframe_request()) {
            encoder_->request_keyframe();
        }
        video::EncodedPacket packet{};
        if (!encoder_->encode(job.texture, packet)) {
            // F5: a replay fault turns the replay off. It never touches the
            // capture that a stream may share.
            MELLO_LOG_ERROR(TAG, "encode failed: the replay is off");
            failed_.store(true, std::memory_order_relaxed);
            return;
        }
        if (packet.data.empty()) continue;
        frames_encoded_.fetch_add(1, std::memory_order_relaxed);
        video_ring_.push(std::move(packet.data), job.capture_us, packet.is_keyframe);
    }
}

void ReplaySession::on_game_audio(const int16_t* samples, size_t count) {
    // 48 kHz stereo. The time of the first frame: the arrival less the length
    // of the packet, as the stream's game audio has it (plan 5.1).
    const size_t frames = count / ReplayAudioRing::kChannels;
    if (frames == 0) return;
    const uint64_t capture_us = now_us() - frames * 1'000'000 / ReplayAudioRing::kRate;
    audio_ring_.push(samples, frames, capture_us);
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
    // A submit, no wait (plan F6). The map on the capture's caller waits.
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
            crew.gain_points = std::move(window.crew_volume);  // Q20
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
    s.frames_encoded = frames_encoded_.load(std::memory_order_relaxed);
    s.encode_queue_drops = queue_drops_.load(std::memory_order_relaxed);
    s.video_ring = video_ring_.stats();
    s.audio_ring = audio_ring_.stats();
    return s;
}

}  // namespace mello::replay
#endif
