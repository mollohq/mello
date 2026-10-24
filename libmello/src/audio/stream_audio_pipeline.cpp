#include "stream_audio_pipeline.hpp"
#include "stream_audio_link.hpp"
#include "../transport/rtp_av_sync_extension.hpp"
#include "../util/log.hpp"
#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdlib>
#include <cstring>

#ifdef _WIN32
#include "capture_wasapi_loopback.hpp"
#endif

namespace mello::audio {

namespace {

int64_t steady_now_us() {
    return std::chrono::duration_cast<std::chrono::microseconds>(
               std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

constexpr int64_t kFrameUs = 20'000;
// A capture time that differs from the running one by more than this is a
// new start (a gap in the source), not jitter.
constexpr int64_t kCaptureRestartUs = 30'000;
// How fast the running capture time follows the measured one when the
// source gives no capture time (arrival times jitter by a device period).
constexpr double kCaptureFollow = 0.02;

} // namespace

StreamAudioHostPipeline::StreamAudioHostPipeline() = default;

StreamAudioHostPipeline::~StreamAudioHostPipeline() {
    stop();
}

void StreamAudioHostPipeline::set_link(std::shared_ptr<StreamAudioLink> link) {
    std::lock_guard<std::mutex> lock(pcm_mutex_);
    link_ = std::move(link);
}

bool StreamAudioHostPipeline::start(PacketCallback callback, uint32_t capture_pid,
                                    bool external_source) {
    if (!callback) return false;
    stop();

#if !defined(_WIN32) && !defined(__APPLE__)
    (void)callback;
    (void)capture_pid;
    (void)external_source;
    MELLO_LOG_WARN("stream_audio", "host pipeline not implemented on this platform");
    return false;
#else

#ifdef _WIN32
    if (!external_source) {
        // Windows opens its own loopback device. macOS has none:
        // ScreenCaptureKit delivers game audio on the video stream, and
        // mello.cpp routes it into feed_float_pcm once this pipeline exists.
        capture_ = std::make_unique<WasapiLoopbackCapture>();
        // Scope the capture so the viewer hears the stream, not the streamer's
        // whole desktop. Capturing a game: that game's process tree. Capturing
        // a monitor: everything except us, so our own voice playback is not fed
        // back to the person speaking.
        const LoopbackScope scope = capture_pid != 0 ? LoopbackScope::IncludeProcessTree
                                                     : LoopbackScope::ExcludeProcessTree;
        const uint32_t target_pid =
            capture_pid != 0 ? capture_pid : static_cast<uint32_t>(GetCurrentProcessId());
        if (!capture_->initialize_scoped(scope, target_pid)) {
            MELLO_LOG_ERROR("stream_audio", "loopback capture init failed");
            capture_.reset();
            return false;
        }
    }
#else
    (void)capture_pid;
    (void)external_source;
#endif

    if (!encoder_.initialize(
            STREAM_AUDIO_SAMPLE_RATE,
            STREAM_AUDIO_CHANNELS,
            STREAM_AUDIO_BITRATE,
            OpusApplication::Audio)) {
        MELLO_LOG_ERROR("stream_audio", "Opus encoder init failed");
#ifdef _WIN32
        capture_.reset();
#endif
        return false;
    }
    // Stage 7: in-band FEC, with the expected loss from the receiver
    // reports (the link), as voice does. The floor is 5 %, so the encoder
    // always spends some bits on it.
    encoder_.set_inband_fec(1);

    {
        std::lock_guard<std::mutex> lock(pcm_mutex_);
        callback_ = std::move(callback);
        pcm_accum_.clear();
        have_accum_capture_ = false;
        if (link_) link_->reset();
        const int perc = link_ ? link_->loss_perc() : kLossPercFloor;
        encoder_.set_packet_loss_perc(perc);
        loss_perc_.store(perc, std::memory_order_relaxed);
    }
    frame_index_ = 0;
    warned_sample_rate_ = false;

#ifdef _WIN32
    if (capture_ && !capture_->start([this](const int16_t* samples, size_t count) {
            on_pcm(samples, count, std::nullopt);
        })) {
        MELLO_LOG_ERROR("stream_audio", "loopback capture start failed");
        std::lock_guard<std::mutex> lock(pcm_mutex_);
        callback_ = nullptr;
        capture_.reset();
        return false;
    }
#endif

    MELLO_LOG_INFO("stream_audio", "host pipeline started (%s, in-band FEC on)",
                   external_source ? "external source" : "own capture");
    return true;
#endif
}

void StreamAudioHostPipeline::stop() {
#ifdef _WIN32
    if (capture_) {
        capture_->stop();
        capture_.reset();
    }
#endif
    // Under the lock: on macOS the ScreenCaptureKit audio queue can still be
    // inside feed_float_pcm while the caller stops the stream.
    std::lock_guard<std::mutex> lock(pcm_mutex_);
    callback_ = nullptr;
    pcm_accum_.clear();
    have_accum_capture_ = false;
}

void StreamAudioHostPipeline::feed_float_pcm(const float* samples, uint32_t frame_count,
                                             uint32_t channels, uint32_t sample_rate) {
    feed_float_pcm_at(samples, frame_count, channels, sample_rate, -1);
}

void StreamAudioHostPipeline::feed_float_pcm_at(const float* samples, uint32_t frame_count,
                                                uint32_t channels, uint32_t sample_rate,
                                                int64_t capture_us) {
    if (!samples || frame_count == 0 || channels == 0) return;

    // The encoder is fixed at 48 kHz and there is no resampler in libmello.
    // ScreenCaptureKit is configured for 48 kHz, so a mismatch means the OS
    // ignored the request — drop rather than emit pitch-shifted audio.
    if (sample_rate != static_cast<uint32_t>(STREAM_AUDIO_SAMPLE_RATE)) {
        if (!warned_sample_rate_) {
            warned_sample_rate_ = true;
            MELLO_LOG_WARN("stream_audio",
                "game audio at %u Hz, expected %d — dropping",
                sample_rate, STREAM_AUDIO_SAMPLE_RATE);
        }
        return;
    }

    // Map onto the encoder's stereo layout: mono duplicates across both sides,
    // more than two channels keeps the front pair rather than downmixing.
    float_conv_.resize(static_cast<size_t>(frame_count) * STREAM_AUDIO_CHANNELS);
    for (uint32_t i = 0; i < frame_count; ++i) {
        for (uint32_t ch = 0; ch < STREAM_AUDIO_CHANNELS; ++ch) {
            const uint32_t src_ch = (channels == 1) ? 0 : ch;
            float v = samples[static_cast<size_t>(i) * channels + src_ch];
            if (v > 1.0f) v = 1.0f;
            if (v < -1.0f) v = -1.0f;
            float_conv_[static_cast<size_t>(i) * STREAM_AUDIO_CHANNELS + ch] =
                static_cast<int16_t>(std::lrintf(v * 32767.0f));
        }
    }

    on_pcm(float_conv_.data(), float_conv_.size(),
           capture_us >= 0 ? std::optional<int64_t>(capture_us) : std::nullopt);
}

void StreamAudioHostPipeline::on_pcm(const int16_t* samples, size_t count,
                                     std::optional<int64_t> capture_us) {
    std::lock_guard<std::mutex> lock(pcm_mutex_);
    if (!callback_ || count == 0) return;

    // Stage 7: the capture time of the first sample in the accumulator. A
    // source with no capture time (the loopback device) gives its arrival
    // time: the last sample of the buffer is captured now.
    const int64_t chunk_frames = static_cast<int64_t>(count / STREAM_AUDIO_CHANNELS);
    const int64_t queued_frames = static_cast<int64_t>(pcm_accum_.size() / STREAM_AUDIO_CHANNELS);
    const bool exact = capture_us.has_value();
    const int64_t chunk_capture_us =
        exact ? *capture_us
              : steady_now_us() - chunk_frames * 1'000'000 / STREAM_AUDIO_SAMPLE_RATE;
    // What the running capture time says about this chunk's first sample.
    const int64_t measured_head = chunk_capture_us - queued_frames * 1'000'000 / STREAM_AUDIO_SAMPLE_RATE;
    if (!have_accum_capture_) {
        accum_capture_us_ = measured_head;
        have_accum_capture_ = true;
    } else {
        const int64_t error = measured_head - accum_capture_us_;
        if (exact || std::llabs(error) > kCaptureRestartUs) {
            accum_capture_us_ = measured_head;
        } else {
            accum_capture_us_ += static_cast<int64_t>(static_cast<double>(error) * kCaptureFollow);
        }
    }

    pcm_accum_.insert(pcm_accum_.end(), samples, samples + count);

    // The expected loss from the receiver reports (stage 7).
    if (link_) {
        const int perc = link_->loss_perc();
        if (perc != loss_perc_.load(std::memory_order_relaxed)) {
            encoder_.set_packet_loss_perc(perc);
            loss_perc_.store(perc, std::memory_order_relaxed);
        }
    }

    const size_t frame_samples =
        static_cast<size_t>(STREAM_AUDIO_FRAME_SAMPLES) * STREAM_AUDIO_CHANNELS;
    while (pcm_accum_.size() >= frame_samples) {
        const int encoded = encoder_.encode(
            pcm_accum_.data(),
            STREAM_AUDIO_FRAME_SAMPLES,
            encode_buf_,
            static_cast<int>(sizeof(encode_buf_)));
        // Media time of this frame. It advances for every frame, sent or
        // not, so the RTP timestamp (ts_us * 48 / 1000 samples) stays on the
        // capture clock.
        const uint64_t ts_us = frame_index_ * 20000;
        ++frame_index_;
        if (link_) {
            // The media time mello-core gives the peer: ts_us in 48 kHz
            // samples, modulo 2^32 (manager.rs media_time_48k).
            link_->record_capture(static_cast<uint32_t>(ts_us * 48 / 1000), accum_capture_us_);
        }
        if (encoded > 0) {
            callback_(encode_buf_, encoded, ts_us);
        } else if (encoded < 0) {
            // encoded == 0 is Opus DTX ("nothing to transmit") and is normal.
            // A negative value is a real encoder error; voice logs it the same
            // way in audio_pipeline.cpp, and without this it is invisible.
            MELLO_LOG_WARN("stream_audio", "opus encode error: %d", encoded);
        }
        pcm_accum_.erase(pcm_accum_.begin(),
                         pcm_accum_.begin() + static_cast<std::ptrdiff_t>(frame_samples));
        accum_capture_us_ += kFrameUs;
    }
}

// ---- Viewer ----

StreamAudioPlayout::StreamAudioPlayout(SourceFactory factory, OutputLatencyFn output_latency)
    : output_latency_(std::move(output_latency)) {
    if (factory) source_ = factory();
    if (!source_) {
        MELLO_LOG_ERROR("stream_audio", "no game-audio source in the voice mix: the stream plays no audio");
    }
}

StreamAudioPlayout::~StreamAudioPlayout() {
    stop();
}

bool StreamAudioPlayout::insert_locked(const RtpInfo& rtp, const uint8_t* payload, size_t size,
                                       int64_t arrival_ms) {
    if (!source_) return false;
    if (!source_->insert(rtp, payload, size, arrival_ms)) return false;
    ++packets_fed_;
    if (packets_fed_ == 1) {
        MELLO_LOG_INFO("stream_audio", "viewer playout: first packet (ssrc %08x) into the voice mix",
                       rtp.ssrc);
    }
    return true;
}

bool StreamAudioPlayout::feed_rtp(const uint8_t* packet, size_t size, int64_t arrival_us) {
    RtpInfo rtp;
    size_t offset = 0;
    size_t payload_size = 0;
    if (!parse_rtp(packet, size, &rtp, &offset, &payload_size)) return false;
    const auto video_ts = transport::read_av_sync_extension(packet, size);
    StartBufferFn start_fn;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (muted_) {
            ++packets_dropped_muted_;
            return true;
        }
        if (video_ts) {
            // The newest packet's mapping: a reordered older one does not
            // move it.
            if (!have_sync_ || static_cast<int32_t>(rtp.timestamp - sync_audio_ts_) >= 0) {
                sync_audio_ts_ = rtp.timestamp;
                sync_video_ts_ = *video_ts;
                have_sync_ = true;
            }
            ++sync_extensions_;
        }
        if (have_ssrc_ && rtp.ssrc != ssrc_) {
            // The host restarted its audio: a new start.
            starting_ = true;
            held_.clear();
        }
        have_ssrc_ = true;
        ssrc_ = rtp.ssrc;
        if (!start_buffer_fn_ || !starting_) {
            // NetEQ's arrival clock is the playout clock in ms (VoicePlayout).
            return insert_locked(rtp, packet + offset, payload_size, arrival_us / 1000);
        }
        held_.push_back(Held{std::vector<uint8_t>(packet, packet + size), rtp.timestamp, arrival_us});
        start_fn = start_buffer_fn_;
    }

    // Outside the lock: the A/V sync takes its own lock, then this one.
    std::optional<int> buffer_ms;
    if (video_ts) {
        const float latency_ms = output_latency_ ? output_latency_() : 0.0f;
        // Inserted now with no buffer, the newest packet plays at the next
        // pull and is heard over the 10 ms after it, then the output.
        const int64_t heard_if_now = arrival_us + 10'000 + static_cast<int64_t>(latency_ms * 1000.0f);
        buffer_ms = start_fn(*video_ts, heard_if_now);
    }

    std::lock_guard<std::mutex> lock(mutex_);
    if (!starting_ || held_.empty()) return true;
    const bool give_up = held_.back().arrival_us - held_.front().arrival_us > 1'000'000;
    if (!buffer_ms && !give_up) {
        // Wait for the video's first frame; keep at most 1 s.
        return true;
    }
    const int keep_ms = buffer_ms.value_or(0);
    const uint32_t newest = held_.back().timestamp;
    const int64_t keep_samples = static_cast<int64_t>(keep_ms) * STREAM_AUDIO_SAMPLE_RATE / 1000;
    if (source_) source_->set_minimum_delay_ms(keep_ms);
    for (const Held& h : held_) {
        if (static_cast<int64_t>(static_cast<int32_t>(newest - h.timestamp)) > keep_samples) continue;
        RtpInfo r;
        size_t off = 0;
        size_t len = 0;
        if (parse_rtp(h.packet.data(), h.packet.size(), &r, &off, &len)) {
            insert_locked(r, h.packet.data() + off, len, h.arrival_us / 1000);
        }
    }
    MELLO_LOG_INFO("stream_audio", "viewer playout starts with a %d ms buffer (%zu packets held%s)",
                   keep_ms, held_.size(), buffer_ms ? "" : ", no video frame in 1 s");
    held_.clear();
    starting_ = false;
    ++start_buffers_;
    last_start_buffer_ms_ = keep_ms;
    return true;
}

void StreamAudioPlayout::set_start_buffer(StartBufferFn fn) {
    std::lock_guard<std::mutex> lock(mutex_);
    start_buffer_fn_ = std::move(fn);
}

bool StreamAudioPlayout::feed_packet(const uint8_t* data, int size) {
    if (!data || size <= 0) return false;
    std::lock_guard<std::mutex> lock(mutex_);
    if (muted_) {
        ++packets_dropped_muted_;
        return true;
    }
    RtpInfo rtp;
    rtp.payload_type = VoicePlayout::kOpusPayloadType;
    rtp.sequence = plain_sequence_++;
    rtp.timestamp = plain_timestamp_;
    plain_timestamp_ += static_cast<uint32_t>(STREAM_AUDIO_FRAME_SAMPLES);
    rtp.ssrc = 0x6D656C6Cu;  // "mell": a fixed SSRC for the header-less path
    return insert_locked(rtp, data, static_cast<size_t>(size), steady_now_us() / 1000);
}

void StreamAudioPlayout::set_muted(bool muted) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (muted_ == muted) return;
    muted_ = muted;
    if (muted && source_) {
        // The queued audio goes, and the sync mapping with it: the next
        // packet after the pause starts a new stream.
        source_->flush();
        have_sync_ = false;
    }
    if (muted) {
        starting_ = true;
        held_.clear();
    }
    MELLO_LOG_INFO("stream_audio", "viewer playout %s", muted ? "muted (flushed)" : "unmuted");
}

bool StreamAudioPlayout::muted() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return muted_;
}

void StreamAudioPlayout::set_volume(float gain) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (source_) source_->set_gain(gain);
}

float StreamAudioPlayout::volume() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return source_ ? source_->gain() : 0.0f;
}

void StreamAudioPlayout::stop() {
    std::unique_ptr<GameAudioSource> source;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        source = std::move(source_);
        have_sync_ = false;
    }
    // Out of the mix outside the lock: it waits for a mix that runs now.
    source.reset();
}

std::optional<transport::AudioSyncSample> StreamAudioPlayout::sync_sample() const {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!source_ || !have_sync_ || muted_) return std::nullopt;
    const auto playout = source_->last_playout();
    if (!playout) return std::nullopt;
    // The audio timestamp that plays, on the video clock: the newest
    // packet's mapping, moved by the difference of the audio timestamps
    // (48 kHz to 90 kHz).
    const int64_t audio_delta = static_cast<int32_t>(playout->rtp_timestamp - sync_audio_ts_);
    const int64_t video_delta = audio_delta * 90'000 / STREAM_AUDIO_SAMPLE_RATE;
    transport::AudioSyncSample s;
    s.video_rtp_timestamp = sync_video_ts_ + static_cast<uint32_t>(video_delta);
    // The pull ended with that sample: it is heard one pull later, after
    // the output ring and the device.
    const float latency_ms = output_latency_ ? output_latency_() : 0.0f;
    if (const auto d = source_->delay()) {
        s.pending_ms = static_cast<double>(d->target_ms - d->filtered_ms);
    }
    s.heard_at_us = playout->pulled_at_us +
                    static_cast<int64_t>(playout->samples) * 1'000'000 / STREAM_AUDIO_SAMPLE_RATE +
                    static_cast<int64_t>(latency_ms * 1000.0f);
    return s;
}

void StreamAudioPlayout::set_sync_delay_ms(int delay_ms) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (source_) source_->set_minimum_delay_ms(delay_ms);
}

StreamAudioPlayout::Stats StreamAudioPlayout::stats() const {
    std::lock_guard<std::mutex> lock(mutex_);
    Stats s;
    s.packets_fed = packets_fed_;
    s.packets_dropped_muted = packets_dropped_muted_;
    s.sync_extensions = sync_extensions_;
    s.start_buffers = start_buffers_;
    s.last_start_buffer_ms = last_start_buffer_ms_;
    if (source_) {
        s.source = source_->stats();
        if (s.source.playing) {
            s.delay_ms = s.source.filtered_delay_ms + (output_latency_ ? output_latency_() : 0.0f);
        }
        s.expand_ms = static_cast<double>(s.source.concealed_samples) * 1000.0 /
                      STREAM_AUDIO_SAMPLE_RATE;
        s.fec_used = s.source.fec_packets_received > s.source.fec_packets_discarded
                         ? s.source.fec_packets_received - s.source.fec_packets_discarded
                         : 0;
    }
    return s;
}

uint64_t StreamAudioPlayout::packets_fed() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return packets_fed_;
}

uint64_t StreamAudioPlayout::packets_dropped_muted() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return packets_dropped_muted_;
}

} // namespace mello::audio
