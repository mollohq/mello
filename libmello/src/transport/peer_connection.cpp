#include "peer_connection_impl.hpp"
#include <chrono>
#include <algorithm>
#include <cstring>
#include <random>

#if RTC_ENABLE_MEDIA
#include <rtc/rtppacketizationconfig.hpp>
#include <rtc/rtppacketizer.hpp>
#include <rtc/rtcpsrreporter.hpp>
#endif

namespace mello::transport {

static uint32_t generate_ssrc() {
    static std::mt19937 rng(std::random_device{}());
    std::uniform_int_distribution<uint32_t> dist(1, 0xFFFFFFFF);
    return dist(rng);
}

PeerConnectionImpl::PeerConnectionImpl(const std::string& peer_id)
    : peer_id_(peer_id)
{
    config_.iceServers.emplace_back("stun:stun.l.google.com:19302");
    config_.iceServers.emplace_back("stun:stun1.l.google.com:19302");
    config_.forceMediaTransport = true;
}

PeerConnectionImpl::~PeerConnectionImpl() {
    try {
        if (pc_) {
            pc_->onLocalDescription(nullptr);
            pc_->onLocalCandidate(nullptr);
            pc_->onStateChange(nullptr);
            pc_->onDataChannel(nullptr);
            pc_->onTrack(nullptr);
            pc_->close();
        }
        for (auto& t : incoming_tracks_) {
            t->onMessage(nullptr);
        }
        incoming_tracks_.clear();
        if (audio_track_) {
            audio_track_->onMessage(nullptr);
            audio_track_.reset();
        }
        if (unreliable_dc_) {
            unreliable_dc_->onMessage(nullptr);
            unreliable_dc_.reset();
        }
        if (reliable_dc_) {
            reliable_dc_->onMessage(nullptr);
            reliable_dc_.reset();
        }
        pc_.reset();
    } catch (...) {}
}

void PeerConnectionImpl::set_ice_servers(const std::vector<std::string>& urls) {
    config_.iceServers.clear();
    for (auto& url : urls) {
        config_.iceServers.emplace_back(url);
    }
}

void PeerConnectionImpl::create_pc() {
    pc_ = std::make_shared<rtc::PeerConnection>(config_);

    pc_->onLocalDescription([this](rtc::Description desc) {
        std::lock_guard<std::mutex> lock(sdp_mutex_);
        local_sdp_ = std::string(desc);
        sdp_ready_ = true;
        sdp_cv_.notify_one();
    });

    pc_->onLocalCandidate([this](rtc::Candidate candidate) {
        if (ice_cb_) {
            MelloIceCandidate mc;
            std::string cand_str = std::string(candidate);
            std::string mid_str = candidate.mid();
            mc.candidate = cand_str.c_str();
            mc.sdp_mid = mid_str.c_str();
            try { mc.sdp_mline_index = std::stoi(mid_str); }
            catch (...) { mc.sdp_mline_index = 0; }
            ice_cb_(ice_ud_, &mc);
        }
    });

    pc_->onStateChange([this](rtc::PeerConnection::State state) {
        connected_ = (state == rtc::PeerConnection::State::Connected);
        if (state_cb_) {
            state_cb_(state_ud_, static_cast<int>(state));
        }
    });

    // Handle incoming tracks from the SFU (one per remote sender)
    pc_->onTrack([this](std::shared_ptr<rtc::Track> track) {
        setup_incoming_track(track);
    });
}

void PeerConnectionImpl::setup_incoming_track(std::shared_ptr<rtc::Track> track) {
    auto mid = track->mid();
    auto desc = track->description();

    // Extract sender user_id from the SDP msid attribute: "msid:<streamID> <trackID>"
    std::string sender_id = "unknown";
    for (const auto& a : desc.attributes()) {
        if (a.rfind("msid:", 0) == 0) {
            auto space = a.find(' ', 5);
            sender_id = (space != std::string::npos) ? a.substr(5, space - 5) : a.substr(5);
            break;
        }
    }

    // Reject phantom tracks created by Pion's undeclared-SSRC handler.
    // Real sender IDs are UUIDs (36 chars, contain dashes). Phantoms have
    // random 16-char cnames with no dashes.
    bool is_phantom = (sender_id.find('-') == std::string::npos);

    fprintf(stderr, "[mello-rtp] onTrack: mid=%s sender=%s open=%d phantom=%d\n",
            mid.c_str(), sender_id.c_str(), track->isOpen() ? 1 : 0, is_phantom ? 1 : 0);
    fflush(stderr);

    if (is_phantom) {
        return;
    }

    track->onOpen([mid]() {
        fprintf(stderr, "[mello-rtp] track OPEN: mid=%s\n", mid.c_str());
        fflush(stderr);
    });
    track->onClosed([mid]() {
        fprintf(stderr, "[mello-rtp] track CLOSED: mid=%s\n", mid.c_str());
        fflush(stderr);
    });
    track->onError([mid](std::string err) {
        fprintf(stderr, "[mello-rtp] track ERROR: mid=%s err=%s\n", mid.c_str(), err.c_str());
        fflush(stderr);
    });

    track->onMessage([this, sender_id](rtc::message_variant data) {
        auto* bin = std::get_if<rtc::binary>(&data);
        if (!bin || bin->size() < 12) return;

        auto* bytes = reinterpret_cast<const uint8_t*>(bin->data());
        int total = static_cast<int>(bin->size());

        // Filter RTCP packets that libdatachannel delivers to onMessage.
        uint8_t pt = bytes[1] & 0x7F;
        if (pt != 111) return;

        // Parse RTP header to extract sequence number and payload
        uint16_t seq = (static_cast<uint16_t>(bytes[2]) << 8)
                     | static_cast<uint16_t>(bytes[3]);

        int cc = bytes[0] & 0x0F;
        int header_len = 12 + cc * 4;

        // Handle RTP header extension
        bool has_ext = (bytes[0] & 0x10) != 0;
        if (has_ext && header_len + 4 <= total) {
            uint16_t ext_len = (static_cast<uint16_t>(bytes[header_len + 2]) << 8)
                             | static_cast<uint16_t>(bytes[header_len + 3]);
            header_len += 4 + ext_len * 4;
        }

        if (header_len >= total) return;

        const uint8_t* opus = bytes + header_len;
        int opus_len = total - header_len;

        // Reconstruct [4B LE seq][Opus payload] for the AudioPipeline
        std::vector<uint8_t> pkt(4 + opus_len);
        pkt[0] = static_cast<uint8_t>(seq);
        pkt[1] = static_cast<uint8_t>(seq >> 8);
        pkt[2] = 0;
        pkt[3] = 0;
        std::memcpy(pkt.data() + 4, opus, opus_len);

        if (audio_track_cb_) {
            audio_track_cb_(audio_track_ud_, sender_id.c_str(),
                           pkt.data(), static_cast<int>(pkt.size()));
        }
    });

    incoming_tracks_.push_back(track);
}

void PeerConnectionImpl::setup_dc_handlers(std::shared_ptr<rtc::DataChannel> dc, bool reliable) {
    dc->onMessage([this, reliable](auto data) {
        if (auto* str = std::get_if<std::string>(&data)) {
            if (reliable && str->size() > 14 && str->substr(0, 14) == R"({"type":"pong")") {
                // Extract "ts": value from pong JSON
                auto pos = str->find("\"ts\":");
                if (pos != std::string::npos) {
                    int64_t sent_ts = std::strtoll(str->c_str() + pos + 5, nullptr, 10);
                    auto now = std::chrono::steady_clock::now();
                    int64_t now_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                        now.time_since_epoch()).count();
                    float rtt = static_cast<float>(now_ms - sent_ts);
                    if (rtt >= 0 && rtt < 10000) {
                        float prev = rtt_ms_.load(std::memory_order_relaxed);
                        float smoothed = (prev < 0.1f) ? rtt : prev * 0.7f + rtt * 0.3f;
                        rtt_ms_.store(smoothed, std::memory_order_relaxed);
                    }
                }
            }
            return;
        }
        if (auto* bin = std::get_if<rtc::binary>(&data)) {
            auto* bytes = reinterpret_cast<const uint8_t*>(bin->data());
            auto size = static_cast<int>(bin->size());

            if (!reliable) {
                std::lock_guard<std::mutex> lock(recv_mutex_);
                if (recv_queue_.size() < MAX_RECV_QUEUE) {
                    recv_queue_.emplace(bytes, bytes + size);
                }
            }
            if (data_cb_) {
                data_cb_(data_ud_, bytes, size, reliable);
            }
        }
    });
}

void PeerConnectionImpl::send_ping() {
    if (!reliable_dc_ || !reliable_dc_->isOpen()) return;
    auto now = std::chrono::steady_clock::now();
    int64_t ts = std::chrono::duration_cast<std::chrono::milliseconds>(
        now.time_since_epoch()).count();
    std::string msg = R"({"type":"ping","ts":)" + std::to_string(ts) + "}";
    try {
        reliable_dc_->send(msg);
    } catch (...) {}
}

void PeerConnectionImpl::setup_channels() {
#if RTC_ENABLE_MEDIA
    // Create an Opus audio track for RTP-based voice (used with SFU)
    rtc::Description::Audio audio("audio", rtc::Description::Direction::SendRecv);
    audio.addOpusCodec(111, "minptime=10;useinbandfec=1");
    audio_track_ = pc_->addTrack(audio);

    auto ssrc = generate_ssrc();
    auto rtpConfig = std::make_shared<rtc::RtpPacketizationConfig>(
        ssrc, "mello", 111, rtc::OpusRtpPacketizer::DefaultClockRate);
    auto packetizer = std::make_shared<rtc::OpusRtpPacketizer>(rtpConfig);
    packetizer->addToChain(std::make_shared<rtc::RtcpSrReporter>(rtpConfig));
    audio_track_->setMediaHandler(packetizer);
#endif

    // Unreliable DataChannel for P2P mesh voice / stream media
    rtc::DataChannelInit dcInit;
    dcInit.reliability.unordered = true;
    dcInit.reliability.maxRetransmits = 0;
    unreliable_dc_ = pc_->createDataChannel("audio", dcInit);
    setup_dc_handlers(unreliable_dc_, false);

    // Control DataChannel (reliable, ordered) -- kept for signaling
    reliable_dc_ = pc_->createDataChannel("control");
    setup_dc_handlers(reliable_dc_, true);
}

const char* PeerConnectionImpl::create_offer() {
    std::lock_guard<std::mutex> lock(mutex_);
    sdp_ready_ = false;
    create_pc();
    setup_channels();

    pc_->setLocalDescription(rtc::Description::Type::Offer);

    {
        std::unique_lock<std::mutex> lk(sdp_mutex_);
        sdp_cv_.wait_for(lk, std::chrono::seconds(5), [this] { return sdp_ready_; });
    }

    return local_sdp_.c_str();
}

const char* PeerConnectionImpl::create_answer(const char* offer_sdp) {
    std::lock_guard<std::mutex> lock(mutex_);
    sdp_ready_ = false;
    create_pc();

    pc_->onDataChannel([this](std::shared_ptr<rtc::DataChannel> dc) {
        auto label = dc->label();
        if (label == "audio") {
            unreliable_dc_ = dc;
            setup_dc_handlers(dc, false);
        } else if (label == "control") {
            reliable_dc_ = dc;
            setup_dc_handlers(dc, true);
        }
    });

    rtc::Description offer(offer_sdp, rtc::Description::Type::Offer);
    pc_->setRemoteDescription(offer);

    {
        std::unique_lock<std::mutex> lk(sdp_mutex_);
        sdp_cv_.wait_for(lk, std::chrono::seconds(5), [this] { return sdp_ready_; });
    }

    return local_sdp_.c_str();
}

const char* PeerConnectionImpl::handle_remote_offer(const char* sdp) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!pc_) return "";

    fprintf(stderr, "[mello-rtp] handle_remote_offer: sdp_len=%zu pc_state=%d\n",
            strlen(sdp), static_cast<int>(pc_->state()));
    fflush(stderr);

    sdp_ready_ = false;

    rtc::Description offer(sdp, rtc::Description::Type::Offer);
    pc_->setRemoteDescription(offer);

    {
        std::unique_lock<std::mutex> lk(sdp_mutex_);
        sdp_cv_.wait_for(lk, std::chrono::seconds(5), [this] { return sdp_ready_; });
    }

    fprintf(stderr, "[mello-rtp] handle_remote_offer: answer ready=%d\n", sdp_ready_ ? 1 : 0);
    fflush(stderr);

    return local_sdp_.c_str();
}

bool PeerConnectionImpl::set_remote_description(const char* sdp, bool is_offer) {
    try {
        auto type = is_offer ? rtc::Description::Type::Offer : rtc::Description::Type::Answer;
        rtc::Description desc(sdp, type);
        pc_->setRemoteDescription(desc);
        return true;
    } catch (...) {
        return false;
    }
}

bool PeerConnectionImpl::add_ice_candidate(const std::string& candidate, const std::string& mid, int /*mline_index*/) {
    try {
        pc_->addRemoteCandidate(rtc::Candidate(candidate, mid));
        return true;
    } catch (...) {
        return false;
    }
}

void PeerConnectionImpl::set_ice_callback(MelloIceCandidateCallback cb, void* user_data) {
    ice_cb_ = cb;
    ice_ud_ = user_data;
}

void PeerConnectionImpl::set_state_callback(MelloPeerStateCallback cb, void* user_data) {
    state_cb_ = cb;
    state_ud_ = user_data;
}

void PeerConnectionImpl::set_data_callback(MelloPeerDataCallback cb, void* user_data) {
    data_cb_ = cb;
    data_ud_ = user_data;
}

void PeerConnectionImpl::set_audio_track_callback(MelloAudioTrackCallback cb, void* user_data) {
    audio_track_cb_ = cb;
    audio_track_ud_ = user_data;
}

bool PeerConnectionImpl::send_unreliable(const uint8_t* data, int size) {
    if (!unreliable_dc_ || !unreliable_dc_->isOpen()) return false;
    try {
        unreliable_dc_->send(reinterpret_cast<const std::byte*>(data), static_cast<size_t>(size));
        return true;
    } catch (...) {
        return false;
    }
}

bool PeerConnectionImpl::send_reliable(const uint8_t* data, int size) {
    if (!reliable_dc_ || !reliable_dc_->isOpen()) return false;
    try {
        reliable_dc_->send(reinterpret_cast<const std::byte*>(data), static_cast<size_t>(size));
        return true;
    } catch (...) {
        return false;
    }
}

bool PeerConnectionImpl::send_audio(const uint8_t* data, int size) {
    bool open = audio_track_ && audio_track_->isOpen();
    if (!open) return false;
    try {
        audio_track_->send(reinterpret_cast<const std::byte*>(data), static_cast<size_t>(size));
        return true;
    } catch (...) {
        return false;
    }
}

bool PeerConnectionImpl::is_connected() const {
    return connected_;
}

int PeerConnectionImpl::recv(uint8_t* buffer, int buffer_size) {
    std::lock_guard<std::mutex> lock(recv_mutex_);
    if (recv_queue_.empty()) return 0;

    auto& front = recv_queue_.front();
    int copy_size = std::min(static_cast<int>(front.size()), buffer_size);
    std::memcpy(buffer, front.data(), copy_size);
    recv_queue_.pop();
    return copy_size;
}

} // namespace mello::transport
