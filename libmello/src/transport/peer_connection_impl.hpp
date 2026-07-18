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
#include "mello.h"
#include "rtp_video_sender.hpp"
#include "rtp_video_receiver_session.hpp"

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
    void set_data_callback(MelloPeerDataCallback cb, void* user_data);
    void set_audio_track_callback(MelloAudioTrackCallback cb, void* user_data);

    bool send_unreliable(const uint8_t* data, int size);
    bool send_reliable(const uint8_t* data, int size);
    bool send_audio(const uint8_t* data, int size);
    bool is_connected() const;
    bool is_unreliable_open() const;
    bool is_reliable_open() const;

    int recv(uint8_t* buffer, int buffer_size);

    const char* handle_remote_offer(const char* sdp);

    bool video_send_access_unit(
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
    bool video_set_receive_target(uint32_t bps) noexcept;
    void video_get_stats(MelloRtpVideoStats* stats) const noexcept;
    bool video_is_open() const noexcept;

    void send_ping();
    float rtt_ms() const { return rtt_ms_.load(std::memory_order_relaxed); }
    int64_t pong_age_ms() const;
    int send_audio_skips() const { return send_audio_count_.load(std::memory_order_relaxed); }
    int recv_track_count() const { return recv_track_count_.load(std::memory_order_relaxed); }

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

    rtc::Description::Video make_stream_video_description(
        rtc::Description::Direction direction,
        const std::string& mid
    ) const;
    bool validate_remote_video_media(
        const rtc::Description::Media& media,
        std::string& error
    ) const;
    std::optional<rtc::Description::Video> prepare_video_for_answer(
        const rtc::Description& offer,
        std::string& error
    );
    bool replace_video_track_for_answer(rtc::Description::Video video);
    void wire_video_track_callbacks(uint64_t generation);
    void try_start_video_pipeline(
        uint64_t expected_pc_generation = 0,
        uint64_t expected_track_generation = 0
    );
    void stop_video_pipeline() noexcept;
    void teardown_video() noexcept;
    void enqueue_video_feedback(VideoFeedbackType type, uint32_t remb_bps = 0) noexcept;
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
    std::string video_cname_;
    uint64_t pacing_target_bps_ = 4'000'000;
    uint32_t receive_target_bps_ = 4'000'000;
    // Remote SDP advertised the TWCC RTP header extension on stream video.
    bool twcc_supported_ = false;

    std::string local_sdp_;
    std::mutex sdp_mutex_;
    std::condition_variable sdp_cv_;
    bool sdp_ready_ = false;
    uint64_t sdp_generation_ = 0;

    MelloIceCandidateCallback ice_cb_ = nullptr;
    void* ice_ud_ = nullptr;
    MelloPeerStateCallback state_cb_ = nullptr;
    void* state_ud_ = nullptr;
    MelloPeerDataCallback data_cb_ = nullptr;
    void* data_ud_ = nullptr;
    MelloAudioTrackCallback audio_track_cb_ = nullptr;
    void* audio_track_ud_ = nullptr;

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

    std::mutex recv_mutex_;
    std::queue<std::vector<uint8_t>> recv_queue_;
    static constexpr size_t MAX_RECV_QUEUE = 200;

};

} // namespace mello::transport
