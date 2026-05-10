#pragma once
#include <rtc/rtc.hpp>
#include <string>
#include <vector>
#include <queue>
#include <mutex>
#include <atomic>
#include <condition_variable>
#include "mello.h"

namespace mello::transport {

class PeerConnectionImpl {
public:
    explicit PeerConnectionImpl(const std::string& peer_id);
    ~PeerConnectionImpl();

    const std::string& peer_id() const { return peer_id_; }

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

    // Poll next received unreliable packet. Returns size copied, 0 if empty.
    int recv(uint8_t* buffer, int buffer_size);

    // Handle a server-initiated renegotiation offer on the existing PC.
    const char* handle_remote_offer(const char* sdp);

private:
    void create_pc();
    void setup_channels();
    void setup_dc_handlers(std::shared_ptr<rtc::DataChannel> dc, bool reliable);
    void setup_incoming_track(std::shared_ptr<rtc::Track> track);

    std::string peer_id_;
    rtc::Configuration config_;
    std::shared_ptr<rtc::PeerConnection> pc_;
    std::shared_ptr<rtc::DataChannel> reliable_dc_;
    std::shared_ptr<rtc::DataChannel> unreliable_dc_;

    // Outgoing audio track (RTP, for SFU voice)
    std::shared_ptr<rtc::Track> audio_track_;

    // Incoming tracks kept alive for their lifecycle
    std::vector<std::shared_ptr<rtc::Track>> incoming_tracks_;

    std::string local_sdp_;
    std::mutex sdp_mutex_;
    std::condition_variable sdp_cv_;
    bool sdp_ready_ = false;

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
    std::mutex mutex_;

public:
    void send_ping();
    float rtt_ms() const { return rtt_ms_.load(std::memory_order_relaxed); }
    int send_audio_skips() const { return send_audio_count_.load(std::memory_order_relaxed); }
    int recv_track_count() const { return recv_track_count_.load(std::memory_order_relaxed); }
private:

    // Thread-safe receive queue for incoming unreliable audio packets
    std::mutex recv_mutex_;
    std::queue<std::vector<uint8_t>> recv_queue_;
    static constexpr size_t MAX_RECV_QUEUE = 200;
};

} // namespace mello::transport
