#include "peer_connection_impl.hpp"
#include <chrono>

namespace mello::transport {

PeerConnectionImpl::PeerConnectionImpl(const std::string& peer_id)
    : peer_id_(peer_id)
{
    config_.iceServers.emplace_back("stun:stun.l.google.com:19302");
}

PeerConnectionImpl::~PeerConnectionImpl() {
    try {
        // Clear callbacks first to prevent use-after-free from libdatachannel threads
        if (pc_) {
            pc_->onLocalDescription(nullptr);
            pc_->onLocalCandidate(nullptr);
            pc_->onStateChange(nullptr);
            pc_->onDataChannel(nullptr);
            pc_->close();
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
            mc.sdp_mline_index = 0;
            ice_cb_(ice_ud_, &mc);
        }
    });

    pc_->onStateChange([this](rtc::PeerConnection::State state) {
        connected_ = (state == rtc::PeerConnection::State::Connected);
        if (state_cb_) {
            state_cb_(state_ud_, static_cast<int>(state));
        }
    });
}

void PeerConnectionImpl::setup_data_channels() {
    // The offerer creates data channels
    rtc::DataChannelInit unreliable_init;
    unreliable_init.reliability.unordered = true;
    unreliable_init.reliability.maxRetransmits = 0;

    unreliable_dc_ = pc_->createDataChannel("audio", unreliable_init);
    reliable_dc_ = pc_->createDataChannel("control");

    auto setup_dc = [this](std::shared_ptr<rtc::DataChannel> dc, bool reliable) {
        dc->onMessage([this, reliable](auto data) {
            if (data_cb_) {
                if (auto* bin = std::get_if<rtc::binary>(&data)) {
                    data_cb_(data_ud_, reinterpret_cast<const uint8_t*>(bin->data()),
                             static_cast<int>(bin->size()), reliable);
                }
            }
        });
    };

    setup_dc(unreliable_dc_, false);
    setup_dc(reliable_dc_, true);
}

const char* PeerConnectionImpl::create_offer() {
    std::lock_guard<std::mutex> lock(mutex_);
    sdp_ready_ = false;
    create_pc();
    setup_data_channels();

    pc_->setLocalDescription(rtc::Description::Type::Offer);

    // Wait for SDP to be gathered
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

    // Set up handler for incoming data channels from the offerer
    pc_->onDataChannel([this](std::shared_ptr<rtc::DataChannel> dc) {
        auto label = dc->label();
        auto setup = [this](std::shared_ptr<rtc::DataChannel> ch, bool reliable) {
            ch->onMessage([this, reliable](auto data) {
                if (data_cb_) {
                    if (auto* bin = std::get_if<rtc::binary>(&data)) {
                        data_cb_(data_ud_, reinterpret_cast<const uint8_t*>(bin->data()),
                                 static_cast<int>(bin->size()), reliable);
                    }
                }
            });
        };

        if (label == "audio") {
            unreliable_dc_ = dc;
            setup(dc, false);
        } else if (label == "control") {
            reliable_dc_ = dc;
            setup(dc, true);
        }
    });

    rtc::Description offer(offer_sdp, rtc::Description::Type::Offer);
    pc_->setRemoteDescription(offer);
    pc_->setLocalDescription(rtc::Description::Type::Answer);

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

bool PeerConnectionImpl::is_connected() const {
    return connected_;
}

} // namespace mello::transport
