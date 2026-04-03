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

    // Extract sender user_id from the SDP msid attribute (stream ID).
    // Pion sets streamID = senderUserID when creating TrackLocalStaticRTP.
    std::string sender_id;
    auto desc = track->description();
    for (const auto& attr : desc.attributes()) {
        // msid attribute format: "msid:<stream_id> <track_id>"
        if (attr.rfind("msid:", 0) == 0) {
            auto space = attr.find(' ', 5);
            sender_id = attr.substr(5, space != std::string::npos ? space - 5 : std::string::npos);
            break;
        }
    }

    if (sender_id.empty()) {
        sender_id = mid;
    }

    track_sender_map_[mid] = sender_id;

    track->onMessage([this, sender_id](rtc::message_variant data) {
        auto* bin = std::get_if<rtc::binary>(&data);
        if (!bin) return;
        auto* bytes = reinterpret_cast<const uint8_t*>(bin->data());
        auto size = static_cast<int>(bin->size());

        if (size < 12) return; // minimum RTP header

        // Parse RTP header to extract 16-bit sequence number and payload
        // Byte 0: V(2) P(1) X(1) CC(4)
        // Byte 1: M(1) PT(7)
        // Bytes 2-3: sequence number (big-endian)
        // Bytes 4-7: timestamp
        // Bytes 8-11: SSRC
        // Bytes 12+: CSRC list (4 bytes each), then payload
        uint8_t cc = bytes[0] & 0x0F;
        bool has_extension = (bytes[0] >> 4) & 0x01;
        int header_len = 12 + cc * 4;

        if (header_len > size) return;

        // Skip RTP header extensions if present
        if (has_extension && header_len + 4 <= size) {
            uint16_t ext_len = (static_cast<uint16_t>(bytes[header_len + 2]) << 8)
                             | static_cast<uint16_t>(bytes[header_len + 3]);
            header_len += 4 + ext_len * 4;
        }

        if (header_len >= size) return;

        uint16_t rtp_seq = (static_cast<uint16_t>(bytes[2]) << 8)
                         | static_cast<uint16_t>(bytes[3]);

        const uint8_t* payload = bytes + header_len;
        int payload_size = size - header_len;

        if (audio_track_cb_) {
            // Reconstruct [4B seq LE][Opus payload] for the audio pipeline
            std::vector<uint8_t> pkt(4 + payload_size);
            uint32_t seq32 = static_cast<uint32_t>(rtp_seq);
            pkt[0] = static_cast<uint8_t>(seq32);
            pkt[1] = static_cast<uint8_t>(seq32 >> 8);
            pkt[2] = 0;
            pkt[3] = 0;
            std::memcpy(pkt.data() + 4, payload, payload_size);

            audio_track_cb_(audio_track_ud_, sender_id.c_str(),
                           pkt.data(), static_cast<int>(pkt.size()));
        }
    });
}

void PeerConnectionImpl::setup_dc_handlers(std::shared_ptr<rtc::DataChannel> dc, bool reliable) {
    dc->onMessage([this, reliable](auto data) {
        if (auto* bin = std::get_if<rtc::binary>(&data)) {
            if (!reliable) {
                std::lock_guard<std::mutex> lock(recv_mutex_);
                if (recv_queue_.size() < MAX_RECV_QUEUE) {
                    recv_queue_.emplace(
                        reinterpret_cast<const uint8_t*>(bin->data()),
                        reinterpret_cast<const uint8_t*>(bin->data()) + bin->size()
                    );
                }
            }
            if (data_cb_) {
                data_cb_(data_ud_, reinterpret_cast<const uint8_t*>(bin->data()),
                         static_cast<int>(bin->size()), reliable);
            }
        }
    });
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

    sdp_ready_ = false;

    rtc::Description offer(sdp, rtc::Description::Type::Offer);
    pc_->setRemoteDescription(offer);

    {
        std::unique_lock<std::mutex> lk(sdp_mutex_);
        sdp_cv_.wait_for(lk, std::chrono::seconds(5), [this] { return sdp_ready_; });
    }

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
    if (!audio_track_ || !audio_track_->isOpen()) return false;
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
