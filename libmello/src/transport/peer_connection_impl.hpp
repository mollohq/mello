#pragma once
#include <rtc/rtc.hpp>
#include <string>
#include <vector>
#include <queue>
#include <deque>
#include <mutex>
#include <atomic>
#include <condition_variable>
#include <memory>
#include <optional>
#include <functional>
#include "mello.h"
#include "av_sync.hpp"
#include "rtp_video_sender.hpp"
#include "rtp_video_receiver_session.hpp"
#include "voice_slot_map.hpp"
#include "voice_send_feedback.hpp"

namespace mello::audio {
class StreamAudioLink;
}

namespace mello::transport {

class PeerConnectionImpl
    : public std::enable_shared_from_this<PeerConnectionImpl> {
public:
    explicit PeerConnectionImpl(const std::string& peer_id,
                                MelloPeerMediaRole role = MELLO_PEER_MEDIA_ROLE_VOICE);
    ~PeerConnectionImpl();

    const std::string& peer_id() const { return peer_id_; }
    MelloPeerMediaRole media_role() const { return role_; }

    void set_ice_servers(const std::vector<std::string>& urls);

    const char* create_offer();
    const char* create_answer(const char* offer_sdp);
    bool set_remote_description(const char* sdp, bool is_offer);
    bool add_ice_candidate(const std::string& candidate, const std::string& mid, int mline_index);

    void set_ice_callback(MelloIceCandidateCallback cb, void* user_data);
    void set_state_callback(MelloPeerStateCallback cb, void* user_data);
    // Runs after a state change and when a DataChannel or the video track
    // opens or closes (mello.h mello_peer_set_transport_callback).
    void set_transport_callback(MelloPeerTransportCallback cb, void* user_data);
    void set_data_callback(MelloPeerDataCallback cb, void* user_data);
    void set_audio_rtp_callback(MelloAudioTrackCallback cb, void* user_data);

    bool send_reliable(const uint8_t* data, int size);
    bool send_audio(const uint8_t* data, int size);
    // One Opus frame with its media time in 48 kHz samples. The RTP
    // timestamp is the packetizer's start timestamp plus `timestamp`.
    bool send_audio_frame(const uint8_t* data, int size, uint32_t timestamp);
    bool is_connected() const;
    bool is_unreliable_open() const;
    bool is_reliable_open() const;

    const char* handle_remote_offer(const char* sdp);

    SendAccessUnitResult video_send_access_unit(
        const uint8_t* data,
        size_t size,
        uint64_t capture_ts_us
    ) noexcept;
    int video_recv_access_unit(
        uint8_t* buffer,
        size_t capacity,
        MelloRtpVideoAccessUnitInfo* info
    );
    bool video_take_feedback(MelloPeerVideoFeedback* feedback) noexcept;
    bool video_set_pacing_target(uint64_t bps) noexcept;
    bool video_set_bitrate_limits(uint64_t min_bps, uint64_t start_bps, uint64_t max_bps) noexcept;
    bool video_set_max_allocated(uint64_t bps) noexcept;
    bool video_set_receive_target(uint32_t bps) noexcept;
    void video_get_stats(MelloRtpVideoStats* stats) const noexcept;
    bool video_is_open() const noexcept;

    // ---- streaming-quality stage 3 ----
    // The receive timeline of the current viewer session, or null. The viewer
    // pipeline polls it on its decode thread. A new session (the video track
    // opened again) has a new timeline: callers ask again when theirs stops.
    std::shared_ptr<VideoReceiveTimeline> video_receive_timeline() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return rtp_video_receiver_ ? rtp_video_receiver_->timeline() : nullptr;
    }
    // ---- end stage 3 ----

    void send_ping();
    float rtt_ms() const { return rtt_ms_.load(std::memory_order_relaxed); }
    int64_t pong_age_ms() const;
    int send_audio_skips() const { return send_audio_count_.load(std::memory_order_relaxed); }
    int recv_track_count() const;

    // Voice SFU receive slots (mello.h mello_peer_set_voice_recv_slots).
    void set_voice_recv_slots(int count);
    // Bind `mid` to `sender_id` ("" frees it) and give the packets held for
    // the mid to the audio callbacks, on the calling thread.
    void set_audio_mid_sender(const std::string& mid, const std::string& sender_id);
    // set_audio_mid_sender at a given steady-clock time, for tests.
    void set_audio_mid_sender_at(const std::string& mid,
                                 const std::string& sender_id,
                                 int64_t now_ms);
    // One RTP packet on voice m-line `mid` at `now_ms`: route it to the mid's
    // sender, or hold it for the mid's `slot` message. The track thread calls
    // it for every packet; tests call it to inject packets.
    // `generation` 0 means the current PeerConnection.
    void route_voice_rtp(const std::string& mid,
                         const uint8_t* rtp,
                         int size,
                         int64_t now_ms,
                         uint64_t generation = 0);
    // Count dropped slot packets in `counter` (the context's
    // MelloDebugStats.rx_unmapped_dropped).
    void set_unmapped_drop_counter(std::shared_ptr<std::atomic<uint64_t>> counter);
    uint64_t unmapped_dropped() const { return voice_slots_.dropped(); }
    void set_control_text_callback(MelloControlTextCallback cb, void* user_data);
    bool send_control_text(const std::string& text);

    // Receiver reports about the audio send track (mello.h
    // MelloSendFeedback, spec 10 section 7.5).
    void set_send_feedback_callback(MelloSendFeedbackCallback cb, void* user_data);
    bool get_send_feedback(MelloSendFeedback* out) const;

    // ---- streaming-quality stage 7 ----
    // Stream viewer: each game-audio RTP packet (the whole packet) with its
    // arrival time on the steady clock, in microseconds, on the network
    // thread, before the audio callbacks. The stream view's playout sets it
    // (mello_stream_viewer_attach_peer). Empty: none.
    using StreamAudioSink = std::function<void(const uint8_t* rtp, size_t size, int64_t arrival_us)>;
    void set_stream_audio_sink(StreamAudioSink sink);
    // Stream viewer: the A/V sync input. Every receiver session of this peer
    // (each video session) runs it. Null: none.
    void set_av_sync(std::shared_ptr<AvSyncInput> input);
    // Stream host: the context's game-audio link (stream_audio_link.hpp). The
    // audio send track puts the A/V sync extension on each frame from it and
    // gives it the receiver reports.
    void set_stream_audio_link(std::shared_ptr<audio::StreamAudioLink> link);
    // Packets the A/V sync extension was put on (host) or read from (viewer).
    uint64_t av_sync_extensions() const {
        return av_sync_extensions_.load(std::memory_order_relaxed);
    }
    // ---- end stage 7 ----

private:
    enum class VideoFeedbackType : uint8_t {
        Pli = 0,
        Remb = 1,
        LocalIdrNeeded = 2,
        GccTarget = 3,
    };

    struct QueuedVideoFeedback {
        VideoFeedbackType type = VideoFeedbackType::Pli;
        uint32_t remb_bitrate_bps = 0;
    };

    // negotiation_mutex_ serializes all SDP operations and PC replacement.
    void create_pc_locked();
    void close_pc_locked() noexcept;
    void recreate_pc_locked();
    void setup_voice_channels();
    void setup_stream_offer_channels();
    void setup_control_dc_answer_handlers();
    void setup_dc_handlers(
        std::shared_ptr<rtc::DataChannel> dc,
        bool reliable,
        uint64_t generation
    );
    void setup_incoming_track(
        std::shared_ptr<rtc::Track> track,
        uint64_t generation
    );
    // `sender_id` names every packet's sender, or, with `by_mid`, the voice
    // slot map names it per packet from the track's mid.
    void wire_incoming_audio_track_callbacks(
        const std::shared_ptr<rtc::Track>& track,
        uint64_t generation,
        const std::string& sender_id,
        bool by_mid = false
    );

    rtc::Description::Video make_stream_video_description(
        rtc::Description::Direction direction,
        const std::string& mid
    ) const;
    rtc::Description::Audio make_stream_audio_description(
        rtc::Description::Direction direction,
        const std::string& mid
    ) const;
    bool validate_remote_video_media(
        const rtc::Description::Media& media,
        std::string& error
    ) const;
    bool validate_remote_audio_media(
        const rtc::Description::Media& media,
        std::string& error
    ) const;
    std::optional<rtc::Description::Video> prepare_video_for_answer(
        const rtc::Description& offer,
        std::string& error
    );
    std::optional<rtc::Description::Audio> prepare_audio_for_answer(
        const rtc::Description& offer,
        std::string& error
    );
    bool replace_video_track_for_answer(rtc::Description::Video video);
    bool replace_audio_track_for_answer(rtc::Description::Audio audio);
    void wire_opus_send_packetizer(const std::shared_ptr<rtc::Track>& track);
    void attach_opus_send_packetizer(const std::shared_ptr<rtc::Track>& track,
                                     uint32_t ssrc,
                                     const std::string& cname);
    bool send_audio_message(const uint8_t* data, int size, const uint32_t* timestamp);
    // libdatachannel thread: one report about the send track arrived.
    void deliver_send_feedback(const MelloSendFeedback& feedback);
    // Give one incoming audio RTP packet of `sender` to the audio callbacks
    // of PeerConnection generation `generation`.
    void deliver_incoming_audio(const std::string& sender,
                                const uint8_t* bytes,
                                int total,
                                uint64_t generation);
    // Call the transport callback (libdatachannel thread). Takes mutex_ to
    // copy it; the caller must not hold mutex_.
    void notify_transport();
    // Log the ICE candidate pair that carries the connection (host, srflx,
    // relay). libdatachannel thread, after the state becomes Connected.
    void log_selected_candidate_pair(const std::shared_ptr<rtc::PeerConnection>& pc) const;
    void wire_video_track_callbacks(uint64_t generation);
    void try_start_video_pipeline(
        uint64_t expected_pc_generation = 0,
        uint64_t expected_track_generation = 0
    );
    void stop_video_pipeline() noexcept;
    void teardown_video() noexcept;
    void enqueue_video_feedback(VideoFeedbackType type, uint32_t remb_bps = 0) noexcept;
    // Smooths a control-channel RTT sample and forwards it as the NACK
    // retry-budget hint to the video receiver session.
    void apply_rtt_sample(float rtt) noexcept;
    void apply_loopback_ice_config();
    void begin_local_sdp_wait(uint64_t generation);
    const char* wait_for_local_sdp(uint64_t generation);
    bool is_current_pc_generation(uint64_t generation) const noexcept;

    std::string peer_id_;
    MelloPeerMediaRole role_ = MELLO_PEER_MEDIA_ROLE_VOICE;
    rtc::Configuration config_;
    std::shared_ptr<rtc::PeerConnection> pc_;
    std::shared_ptr<rtc::DataChannel> reliable_dc_;
    std::shared_ptr<rtc::DataChannel> unreliable_dc_;

    std::shared_ptr<rtc::Track> audio_track_;
    std::shared_ptr<rtc::Track> video_track_;
    std::unique_ptr<RtpVideoSender> rtp_video_sender_;
    std::unique_ptr<RtpVideoReceiverSession> rtp_video_receiver_;
    std::vector<std::shared_ptr<rtc::Track>> incoming_tracks_;

    uint32_t video_ssrc_ = 0;
    // RFC 4588 retransmission SSRC of the host's video, offered with
    // video_ssrc_ in an ssrc-group FID.
    uint32_t video_rtx_ssrc_ = 0;
    std::string video_cname_;
    uint32_t audio_ssrc_ = 0;
    std::string audio_cname_;
    // Start timestamp of the send track's RTP packetizer (random per
    // packetizer). send_audio_frame adds the frame's media time to it.
    std::atomic<uint32_t> audio_rtp_start_timestamp_{0};
    uint64_t pacing_target_bps_ = 4'000'000;
    uint32_t receive_target_bps_ = 4'000'000;
    // Remote SDP advertised the TWCC RTP header extension on stream video.
    bool twcc_supported_ = false;
    // Remote SDP advertised the ULPFEC payload type (127) on stream video.
    // Only the viewer's receive session uses it; the host sends no FEC.
    bool fec_supported_ = false;
    // RTX payload type of the remote answer (apt 96), 0 when not negotiated.
    uint8_t rtx_payload_type_ = 0;
    // goog_cc limits for the sender (mello_peer_video_set_bitrate_limits).
    uint64_t min_bitrate_bps_ = 300'000;
    uint64_t start_bitrate_bps_ = 0;
    uint64_t max_bitrate_bps_ = 0;

    std::string local_sdp_;
    std::mutex sdp_mutex_;
    std::condition_variable sdp_cv_;
    bool sdp_ready_ = false;
    uint64_t sdp_generation_ = 0;

    MelloIceCandidateCallback ice_cb_ = nullptr;
    void* ice_ud_ = nullptr;
    MelloPeerStateCallback state_cb_ = nullptr;
    void* state_ud_ = nullptr;
    MelloPeerTransportCallback transport_cb_ = nullptr;
    void* transport_ud_ = nullptr;
    MelloPeerDataCallback data_cb_ = nullptr;
    void* data_ud_ = nullptr;
    // Whole RTP packets for the voice and stream audio receive paths.
    MelloAudioTrackCallback audio_rtp_cb_ = nullptr;
    void* audio_rtp_ud_ = nullptr;
    // Control-channel text messages other than pong.
    MelloControlTextCallback control_text_cb_ = nullptr;
    void* control_text_ud_ = nullptr;
    // Receiver reports about the audio send track. The tracker lives as
    // long as the peer: a new send track (renegotiation) keeps counting.
    // send_feedback_cb_mutex_ guards the callback and its user data, and
    // deliver_send_feedback() holds it for the whole call, so a clear
    // waits for a running call. Lock order: send_feedback_cb_mutex_ first,
    // then the locks that the callback takes (in mello-core: the voice
    // sink read lock, then AudioPipeline::adapt_mutex_).
    std::mutex send_feedback_cb_mutex_;
    MelloSendFeedbackCallback send_feedback_cb_ = nullptr;
    void* send_feedback_ud_ = nullptr;
    std::shared_ptr<SendFeedbackTracker> send_feedback_ =
        std::make_shared<SendFeedbackTracker>();

    // Recvonly receive slots in the next voice offer. Set before
    // create_offer, read under negotiation_mutex_.
    std::atomic<int> voice_recv_slots_{0};
    // Voice: mid -> sender user id, from the SFU's slot messages, and the
    // packets held for a mid with no owner yet.
    VoiceSlotMap voice_slots_;
    // Orders slot packet delivery: the held packets of a bind reach the
    // callbacks before a newer packet of the same mid. Taken before mutex_,
    // never inside it.
    std::mutex slot_feed_mutex_;

    std::atomic<bool> connected_{false};
    std::atomic<bool> unreliable_open_{false};
    std::atomic<bool> reliable_open_{false};
    std::atomic<int> send_audio_count_{0};
    std::atomic<int> recv_track_count_{0};
    std::atomic<float> rtt_ms_{0.0f};
    std::atomic<int64_t> last_pong_ts_ms_{0};
    mutable std::mutex mutex_;
    std::mutex negotiation_mutex_;
    std::atomic<uint64_t> pc_generation_{0};
    std::atomic<uint64_t> video_track_generation_{0};

    std::mutex feedback_mutex_;
    std::deque<QueuedVideoFeedback> feedback_queue_;
    static constexpr size_t kMaxFeedbackQueue = 64;

    // ---- streaming-quality stage 7 ----
    // The A/V sync extension value of the frame that the audio send path
    // sends now: send_audio_message sets it, the extension handler of the
    // send chain reads it on the same call (Track::send runs the chain on
    // the calling thread). Guarded by av_sync_send_mutex_, a leaf.
    struct PendingAvSync {
        uint32_t rtp_timestamp = 0;
        uint32_t video_rtp_timestamp = 0;
        bool valid = false;
    };
    std::optional<uint32_t> take_pending_av_sync(uint32_t rtp_timestamp);
    std::mutex av_sync_send_mutex_;
    PendingAvSync pending_av_sync_;
    // Guarded by mutex_.
    StreamAudioSink stream_audio_sink_;
    std::shared_ptr<AvSyncInput> av_sync_input_;
    std::shared_ptr<audio::StreamAudioLink> stream_audio_link_;
    std::atomic<uint64_t> av_sync_extensions_{0};
    // ---- end stage 7 ----
};

} // namespace mello::transport
