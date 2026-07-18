#include "peer_connection_impl.hpp"

#include "twcc.hpp"

#include <chrono>
#include <algorithm>
#include <cstring>
#include <limits>
#include <random>

#if RTC_ENABLE_MEDIA
#include <rtc/rtppacketizationconfig.hpp>
#include <rtc/rtppacketizer.hpp>
#include <rtc/rtcpsrreporter.hpp>
#endif

namespace mello::transport {
namespace {

constexpr uint8_t kVideoPayloadType = 96;
constexpr char kVideoMid[] = "video";
constexpr char kVideoFmtp[] =
    "profile-level-id=4d002a;packetization-mode=1;level-asymmetry-allowed=1";

uint32_t generate_ssrc() {
    thread_local std::mt19937 rng(std::random_device{}());
    std::uniform_int_distribution<uint32_t> dist(1, 0xFFFFFFFF);
    return dist(rng);
}

std::string generate_cname() {
    static std::atomic<uint32_t> counter{1};
    return "mello-" + std::to_string(counter.fetch_add(1, std::memory_order_relaxed));
}

bool is_stream_role(MelloPeerMediaRole role) noexcept {
    return role == MELLO_PEER_MEDIA_ROLE_STREAM_HOST
        || role == MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER;
}

bool direction_is_send(rtc::Description::Direction direction) noexcept {
    return direction == rtc::Description::Direction::SendOnly
        || direction == rtc::Description::Direction::SendRecv;
}

bool direction_is_receive(rtc::Description::Direction direction) noexcept {
    return direction == rtc::Description::Direction::RecvOnly
        || direction == rtc::Description::Direction::SendRecv;
}

bool is_h264_format(const std::string& format) noexcept {
    return format.size() == 4
        && (format[0] == 'H' || format[0] == 'h')
        && format[1] == '2'
        && format[2] == '6'
        && format[3] == '4';
}

bool fmtp_has_kv(const rtc::Description::Media::RtpMap& map,
                 const std::string& key,
                 const std::string& value) {
    const std::string needle = key + "=" + value;
    for (const auto& fmtp : map.fmtps) {
        if (fmtp == needle) {
            return true;
        }
    }

    for (const auto& fmtp : map.fmtps) {
        size_t offset = 0;
        while (offset < fmtp.size()) {
            const size_t end = fmtp.find(';', offset);
            const std::string part = fmtp.substr(
                offset,
                end == std::string::npos ? std::string::npos : end - offset
            );
            if (part == needle) {
                return true;
            }
            if (end == std::string::npos) {
                break;
            }
            offset = end + 1;
        }
    }
    return false;
}

size_t video_media_count(const rtc::Description& description) {
    size_t count = 0;
    for (int index = 0; index < description.mediaCount(); ++index) {
        const auto media = description.media(index);
        if (const auto* entry = std::get_if<const rtc::Description::Media*>(&media)) {
            if ((*entry)->type() == "video") {
                ++count;
            }
        }
    }
    return count;
}

const rtc::Description::Media* find_single_video_media(
    const rtc::Description& description,
    std::string& error
) {
    const rtc::Description::Media* video = nullptr;
    for (int index = 0; index < description.mediaCount(); ++index) {
        const auto media = description.media(index);
        if (const auto* entry = std::get_if<const rtc::Description::Media*>(&media)) {
            if ((*entry)->type() == "video") {
                if (video != nullptr) {
                    error = "remote offer must contain exactly one video media section";
                    return nullptr;
                }
                video = *entry;
            }
        }
    }
    if (video == nullptr) {
        error = "remote offer must contain exactly one video media section";
    }
    return video;
}

// True when the remote video media section advertises the TWCC RTP header
// extension (id is fixed on both ends; the URI is the match criterion).
bool remote_media_supports_twcc(const rtc::Description::Media& media) {
    try {
        const auto* map = media.extMap(kTwccExtensionId);
        return map != nullptr
            && map->uri == kTwccExtensionUri;
    } catch (...) {
        return false;
    }
}

int media_index_for_mid(
    const rtc::Description& description,
    const std::string& mid
) noexcept {
    try {
        for (int index = 0; index < description.mediaCount(); ++index) {
            const auto media = description.media(index);
            if (const auto* entry =
                    std::get_if<const rtc::Description::Media*>(&media)) {
                if ((*entry)->mid() == mid) {
                    return index;
                }
            } else if (const auto* entry =
                           std::get_if<const rtc::Description::Application*>(&media)) {
                if ((*entry)->mid() == mid) {
                    return index;
                }
            }
        }
    } catch (...) {
    }
    return -1;
}

} // namespace

PeerConnectionImpl::PeerConnectionImpl(const std::string& peer_id,
                                       MelloPeerMediaRole role)
    : peer_id_(peer_id),
      role_(role)
{
    config_.iceServers.emplace_back("stun:stun.l.google.com:19302");
    config_.iceServers.emplace_back("stun:stun1.l.google.com:19302");
    config_.forceMediaTransport = true;
}

PeerConnectionImpl::~PeerConnectionImpl() {
    try {
        std::lock_guard<std::mutex> negotiation_lock(negotiation_mutex_);
        close_pc_locked();
    } catch (...) {
    }
}

void PeerConnectionImpl::apply_loopback_ice_config() {
    if (!config_.iceServers.empty()) {
        return;
    }

    config_.bindAddress = "127.0.0.1";
    static std::atomic<uint16_t> port_base{50000};
    const uint16_t base = port_base.fetch_add(100, std::memory_order_relaxed);
    config_.portRangeBegin = base;
    config_.portRangeEnd = static_cast<uint16_t>(base + 99);
}

void PeerConnectionImpl::set_ice_servers(const std::vector<std::string>& urls) {
    std::lock_guard<std::mutex> negotiation_lock(negotiation_mutex_);
    config_.iceServers.clear();
    for (const auto& url : urls) {
        config_.iceServers.emplace_back(url);
    }
    apply_loopback_ice_config();
}

bool PeerConnectionImpl::is_current_pc_generation(
    uint64_t generation
) const noexcept {
    return generation != 0
        && pc_generation_.load(std::memory_order_acquire) == generation;
}

void PeerConnectionImpl::close_pc_locked() noexcept {
    pc_generation_.fetch_add(1, std::memory_order_acq_rel);
    teardown_video();

    std::shared_ptr<rtc::PeerConnection> pc;
    std::shared_ptr<rtc::DataChannel> reliable;
    std::shared_ptr<rtc::DataChannel> unreliable;
    std::shared_ptr<rtc::Track> audio;
    std::vector<std::shared_ptr<rtc::Track>> incoming;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        pc = std::move(pc_);
        reliable = std::move(reliable_dc_);
        unreliable = std::move(unreliable_dc_);
        audio = std::move(audio_track_);
        incoming = std::move(incoming_tracks_);
        connected_.store(false, std::memory_order_release);
        reliable_open_.store(false, std::memory_order_release);
        unreliable_open_.store(false, std::memory_order_release);
    }

    try {
        for (auto& track : incoming) {
            track->resetCallbacks();
            track->close();
        }
        if (audio) {
            audio->resetCallbacks();
            audio->close();
        }
        if (unreliable) {
            unreliable->resetCallbacks();
            unreliable->close();
        }
        if (reliable) {
            reliable->resetCallbacks();
            reliable->close();
        }
        if (pc) {
            pc->resetCallbacks();
            pc->close();
        }
    } catch (...) {
    }
}

void PeerConnectionImpl::recreate_pc_locked() {
    close_pc_locked();
    create_pc_locked();
}

void PeerConnectionImpl::create_pc_locked() {
    apply_loopback_ice_config();
    const uint64_t generation =
        pc_generation_.load(std::memory_order_acquire);
    auto pc = std::make_shared<rtc::PeerConnection>(config_);
    const std::weak_ptr<PeerConnectionImpl> weak_self = weak_from_this();
    const std::weak_ptr<rtc::PeerConnection> weak_pc = pc;

    {
        std::lock_guard<std::mutex> lock(mutex_);
        pc_ = pc;
    }

    pc->onLocalDescription([weak_self, generation](rtc::Description desc) {
        const auto self = weak_self.lock();
        if (!self) {
            return;
        }
        std::lock_guard<std::mutex> lock(self->sdp_mutex_);
        if (!self->is_current_pc_generation(generation)
            || self->sdp_generation_ != generation) {
            return;
        }
        self->local_sdp_ = std::string(desc);
        self->sdp_ready_ = true;
        self->sdp_cv_.notify_one();
    });

    pc->onLocalCandidate([weak_self, weak_pc, generation](rtc::Candidate candidate) {
        const auto self = weak_self.lock();
        const auto callback_pc = weak_pc.lock();
        if (!self || !callback_pc
            || !self->is_current_pc_generation(generation)) {
            return;
        }

        MelloIceCandidateCallback callback = nullptr;
        void* user_data = nullptr;
        {
            std::lock_guard<std::mutex> lock(self->mutex_);
            if (!self->is_current_pc_generation(generation)) {
                return;
            }
            callback = self->ice_cb_;
            user_data = self->ice_ud_;
        }
        if (!callback) {
            return;
        }

        const std::string candidate_string(candidate);
        const std::string mid = candidate.mid();
        int mline_index = -1;
        if (const auto local = callback_pc->localDescription()) {
            mline_index = media_index_for_mid(*local, mid);
        }
        const MelloIceCandidate mello_candidate{
            candidate_string.c_str(),
            mid.c_str(),
            mline_index
        };
        callback(user_data, &mello_candidate);
    });

    pc->onStateChange([weak_self, generation](rtc::PeerConnection::State state) {
        const auto self = weak_self.lock();
        if (!self) {
            return;
        }

        MelloPeerStateCallback callback = nullptr;
        void* user_data = nullptr;
        const bool connected = state == rtc::PeerConnection::State::Connected;
        {
            std::lock_guard<std::mutex> lock(self->mutex_);
            if (!self->is_current_pc_generation(generation)) {
                return;
            }
            self->connected_.store(connected, std::memory_order_release);
            if (!connected) {
                self->unreliable_open_.store(false, std::memory_order_release);
                self->reliable_open_.store(false, std::memory_order_release);
            }
            callback = self->state_cb_;
            user_data = self->state_ud_;
        }
        if (connected && is_stream_role(self->role_)) {
            self->try_start_video_pipeline(generation);
        }
        if (callback) {
            callback(user_data, static_cast<int>(state));
        }
    });

    pc->onTrack([weak_self, generation](std::shared_ptr<rtc::Track> track) {
        const auto self = weak_self.lock();
        if (self && self->is_current_pc_generation(generation)
            && self->role_ == MELLO_PEER_MEDIA_ROLE_VOICE) {
            self->setup_incoming_track(std::move(track), generation);
        }
    });
}

rtc::Description::Video PeerConnectionImpl::make_stream_video_description(
    rtc::Description::Direction direction,
    const std::string& mid
) const {
    rtc::Description::Video video(mid, direction);
    video.addH264Codec(kVideoPayloadType, kVideoFmtp);
    if (auto* map = video.rtpMap(kVideoPayloadType)) {
        map->addFeedback("nack");
        map->addFeedback("nack pli");
        map->addFeedback("goog-remb");
        map->addFeedback("transport-cc");
    }
    video.addExtMap(rtc::Description::Media::ExtMap(
        kTwccExtensionId,
        kTwccExtensionUri
    ));
    if (direction_is_send(direction)) {
        const uint32_t ssrc = video_ssrc_ != 0 ? video_ssrc_ : generate_ssrc();
        const std::string cname = video_cname_.empty() ? generate_cname() : video_cname_;
        video.addSSRC(ssrc, cname, peer_id_, "video");
    }
    return video;
}

bool PeerConnectionImpl::validate_remote_video_media(
    const rtc::Description::Media& media,
    std::string& error
) const {
    if (!media.hasPayloadType(kVideoPayloadType)) {
        error = "remote SDP missing H264 payload type 96";
        return false;
    }

    const auto* map = media.rtpMap(kVideoPayloadType);
    if (map == nullptr || !is_h264_format(map->format)) {
        error = "remote SDP payload type 96 is not H264";
        return false;
    }
    if (map->clockRate != 90'000) {
        error = "remote SDP H264 clock rate must be 90000";
        return false;
    }
    if (!fmtp_has_kv(*map, "profile-level-id", "4d002a")
        || !fmtp_has_kv(*map, "packetization-mode", "1")
        || !fmtp_has_kv(*map, "level-asymmetry-allowed", "1")) {
        error = "remote SDP H264 fmtp is incompatible";
        return false;
    }
    return true;
}

std::optional<rtc::Description::Video>
PeerConnectionImpl::prepare_video_for_answer(
    const rtc::Description& offer,
    std::string& error
) {
    const auto* remote_video = find_single_video_media(offer, error);
    if (remote_video == nullptr) {
        return std::nullopt;
    }

    const auto remote_direction = remote_video->direction();
    rtc::Description::Direction local_direction =
        rtc::Description::Direction::Inactive;

    if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_HOST) {
        if (remote_direction != rtc::Description::Direction::RecvOnly) {
            error = "stream host answer requires recvonly remote video";
            return std::nullopt;
        }
        local_direction = rtc::Description::Direction::SendOnly;
        if (video_ssrc_ == 0) {
            video_ssrc_ = generate_ssrc();
        }
        if (video_cname_.empty()) {
            video_cname_ = generate_cname();
        }
    } else if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER) {
        if (remote_direction != rtc::Description::Direction::SendOnly) {
            error = "stream viewer answer requires sendonly remote video";
            return std::nullopt;
        }
        local_direction = rtc::Description::Direction::RecvOnly;
    } else {
        error = "video answer requested for non-stream role";
        return std::nullopt;
    }

    if (!validate_remote_video_media(*remote_video, error)) {
        return std::nullopt;
    }

    return make_stream_video_description(
        local_direction,
        remote_video->mid()
    );
}

bool PeerConnectionImpl::replace_video_track_for_answer(
    rtc::Description::Video video
) {
    std::shared_ptr<rtc::PeerConnection> pc;
    std::shared_ptr<rtc::Track> existing;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        pc = pc_;
        existing = video_track_;
    }
    if (!pc) {
        return false;
    }
    if (existing && !existing->isClosed() && existing->mid() != video.mid()) {
        return false;
    }

    stop_video_pipeline();

    // libdatachannel 0.24.1 addTrack() deliberately reuses a live track with
    // the same mid and replaces its description instead of adding an m-line.
    auto replacement = pc->addTrack(std::move(video));
    if (!replacement) {
        return false;
    }
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (pc_ != pc) {
            return false;
        }
        video_track_ = replacement;
    }
    wire_video_track_callbacks(
        pc_generation_.load(std::memory_order_acquire)
    );
    return true;
}

void PeerConnectionImpl::wire_video_track_callbacks(uint64_t generation) {
    std::shared_ptr<rtc::Track> track;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        track = video_track_;
    }
    if (!track) {
        return;
    }

    const uint64_t track_generation =
        video_track_generation_.fetch_add(1, std::memory_order_acq_rel) + 1;
    const std::weak_ptr<PeerConnectionImpl> weak_self = weak_from_this();
    track->onOpen([weak_self, generation, track_generation]() {
        const auto self = weak_self.lock();
        if (self) {
            self->try_start_video_pipeline(generation, track_generation);
        }
    });
}

void PeerConnectionImpl::try_start_video_pipeline(
    uint64_t expected_pc_generation,
    uint64_t expected_track_generation
) {
    const uint64_t generation =
        pc_generation_.load(std::memory_order_acquire);
    const uint64_t track_generation =
        video_track_generation_.load(std::memory_order_acquire);
    if ((expected_pc_generation != 0 && expected_pc_generation != generation)
        || (expected_track_generation != 0
            && expected_track_generation != track_generation)) {
        return;
    }

    std::shared_ptr<rtc::Track> track;
    std::unique_ptr<RtpVideoSender> failed_sender;
    std::unique_ptr<RtpVideoReceiverSession> failed_receiver;
    uint32_t sender_ssrc = 0;
    std::string sender_cname;
    uint64_t pacing_target_bps = 0;
    uint32_t receive_target_bps = 0;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (!is_current_pc_generation(generation)
            || video_track_generation_.load(std::memory_order_acquire)
                != track_generation) {
            return;
        }
        track = video_track_;
        if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_HOST) {
            if (video_ssrc_ == 0) {
                video_ssrc_ = generate_ssrc();
            }
            if (video_cname_.empty()) {
                video_cname_ = generate_cname();
            }
            sender_ssrc = video_ssrc_;
            sender_cname = video_cname_;
            pacing_target_bps = pacing_target_bps_;
        } else if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER) {
            receive_target_bps = receive_target_bps_;
        }
        if (rtp_video_sender_) {
            if (rtp_video_sender_->is_open()) {
                return;
            }
            failed_sender = std::move(rtp_video_sender_);
        }
        if (rtp_video_receiver_) {
            if (rtp_video_receiver_->is_open()) {
                return;
            }
            failed_receiver = std::move(rtp_video_receiver_);
        }
    }
    failed_sender.reset();
    failed_receiver.reset();

    if (!track || track->isClosed() || !track->isOpen()) {
        return;
    }

    if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_HOST) {
        RtpVideoSenderConfig config;
        config.ssrc = sender_ssrc;
        config.payload_type = kVideoPayloadType;
        config.cname = std::move(sender_cname);
        config.pacing_target_bps = pacing_target_bps;
        config.twcc_enabled = twcc_supported_;

        const std::weak_ptr<PeerConnectionImpl> weak_self = weak_from_this();
        auto sender = std::make_unique<RtpVideoSender>(
            track,
            config,
            [weak_self, generation, track_generation]() {
                const auto self = weak_self.lock();
                if (self && self->is_current_pc_generation(generation)
                    && self->video_track_generation_.load(
                           std::memory_order_acquire
                       ) == track_generation) {
                    self->enqueue_video_feedback(VideoFeedbackType::Pli);
                }
            },
            [weak_self, generation, track_generation](uint32_t bitrate_bps) {
                const auto self = weak_self.lock();
                if (self && self->is_current_pc_generation(generation)
                    && self->video_track_generation_.load(
                           std::memory_order_acquire
                       ) == track_generation) {
                    self->enqueue_video_feedback(
                        VideoFeedbackType::Remb,
                        bitrate_bps
                    );
                }
            },
            [weak_self, generation, track_generation]() {
                const auto self = weak_self.lock();
                if (self && self->is_current_pc_generation(generation)
                    && self->video_track_generation_.load(
                           std::memory_order_acquire
                       ) == track_generation) {
                    self->enqueue_video_feedback(
                        VideoFeedbackType::LocalIdrNeeded
                    );
                }
            },
            [weak_self, generation, track_generation](uint32_t bitrate_bps) {
                const auto self = weak_self.lock();
                if (self && self->is_current_pc_generation(generation)
                    && self->video_track_generation_.load(
                           std::memory_order_acquire
                       ) == track_generation) {
                    self->enqueue_video_feedback(
                        VideoFeedbackType::GccTarget,
                        bitrate_bps
                    );
                }
            }
        );
        if (!sender->is_open()) {
            return;
        }
        std::lock_guard<std::mutex> lock(mutex_);
        if (is_current_pc_generation(generation)
            && video_track_generation_.load(std::memory_order_acquire)
                == track_generation
            && video_track_ == track
            && !rtp_video_sender_) {
            rtp_video_sender_ = std::move(sender);
        }
        return;
    }

    if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER) {
        RtpVideoReceiverSessionConfig config;
        config.payload_type = kVideoPayloadType;
        config.twcc_enabled = twcc_supported_;
        auto receiver = std::make_unique<RtpVideoReceiverSession>(
            track,
            config
        );
        if (!receiver->is_open()
            || (receive_target_bps != 0
                && !receiver->set_receive_target(receive_target_bps))) {
            return;
        }
        std::lock_guard<std::mutex> lock(mutex_);
        if (is_current_pc_generation(generation)
            && video_track_generation_.load(std::memory_order_acquire)
                == track_generation
            && video_track_ == track
            && !rtp_video_receiver_) {
            rtp_video_receiver_ = std::move(receiver);
        }
    }
}

void PeerConnectionImpl::stop_video_pipeline() noexcept {
    std::unique_ptr<RtpVideoSender> sender;
    std::unique_ptr<RtpVideoReceiverSession> receiver;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        video_track_generation_.fetch_add(1, std::memory_order_acq_rel);
        sender = std::move(rtp_video_sender_);
        receiver = std::move(rtp_video_receiver_);
    }
    sender.reset();
    receiver.reset();
    {
        std::lock_guard<std::mutex> lock(feedback_mutex_);
        feedback_queue_.clear();
    }
}

void PeerConnectionImpl::teardown_video() noexcept {
    stop_video_pipeline();
    std::shared_ptr<rtc::Track> track;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        video_track_generation_.fetch_add(1, std::memory_order_acq_rel);
        track = std::move(video_track_);
    }
    if (track) {
        try {
            track->resetCallbacks();
            track->close();
        } catch (...) {
        }
    }
}

void PeerConnectionImpl::enqueue_video_feedback(
    VideoFeedbackType type,
    uint32_t remb_bps
) noexcept {
    try {
        std::lock_guard<std::mutex> lock(feedback_mutex_);
        if (feedback_queue_.size() >= kMaxFeedbackQueue) {
            feedback_queue_.pop_front();
        }
        feedback_queue_.push_back({type, remb_bps});
    } catch (...) {
    }
}

void PeerConnectionImpl::setup_incoming_track(
    std::shared_ptr<rtc::Track> track,
    uint64_t generation
) {
    const auto mid = track->mid();
    const auto desc = track->description();

    std::string sender_id = "unknown";
    for (const auto& attribute : desc.attributes()) {
        if (attribute.rfind("msid:", 0) == 0) {
            const auto space = attribute.find(' ', 5);
            sender_id = (space != std::string::npos)
                ? attribute.substr(5, space - 5)
                : attribute.substr(5);
            break;
        }
    }

    const bool is_phantom = (sender_id.find('-') == std::string::npos);
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
        fprintf(stderr, "[mello-rtp] track ERROR: mid=%s err=%s\n",
                mid.c_str(), err.c_str());
        fflush(stderr);
    });

    auto msg_count = std::make_shared<std::atomic<int>>(0);
    const std::weak_ptr<PeerConnectionImpl> weak_self = weak_from_this();
    track->onMessage(
        [weak_self, generation, sender_id, msg_count](rtc::message_variant data) {
        const auto self = weak_self.lock();
        if (!self || !self->is_current_pc_generation(generation)) {
            return;
        }
        const auto* bin = std::get_if<rtc::binary>(&data);
        if (!bin || bin->size() < 12) {
            return;
        }

        const auto* bytes = reinterpret_cast<const uint8_t*>(bin->data());
        const int total = static_cast<int>(bin->size());

        const int n = msg_count->fetch_add(1, std::memory_order_relaxed) + 1;
        if (n <= 3 || n == 50 || n == 500) {
            fprintf(stderr, "[mello-rtp] onMessage #%d: sender=%s size=%d\n",
                    n, sender_id.c_str(), total);
            fflush(stderr);
        }

        const uint8_t pt = bytes[1] & 0x7F;
        if (pt != 111) {
            return;
        }

        const uint16_t seq = static_cast<uint16_t>(
            (static_cast<uint16_t>(bytes[2]) << 8)
            | static_cast<uint16_t>(bytes[3])
        );

        int cc = bytes[0] & 0x0F;
        int header_len = 12 + cc * 4;

        const bool has_ext = (bytes[0] & 0x10) != 0;
        if (has_ext && header_len + 4 <= total) {
            const uint16_t ext_len = static_cast<uint16_t>(
                (static_cast<uint16_t>(bytes[header_len + 2]) << 8)
                | static_cast<uint16_t>(bytes[header_len + 3])
            );
            header_len += 4 + ext_len * 4;
        }

        if (header_len >= total) {
            return;
        }

        const uint8_t* opus = bytes + header_len;
        const int opus_len = total - header_len;

        std::vector<uint8_t> pkt(4 + opus_len);
        pkt[0] = static_cast<uint8_t>(seq);
        pkt[1] = static_cast<uint8_t>(seq >> 8);
        pkt[2] = 0;
        pkt[3] = 0;
        std::memcpy(pkt.data() + 4, opus, opus_len);

        MelloAudioTrackCallback callback = nullptr;
        void* user_data = nullptr;
        {
            std::lock_guard<std::mutex> lock(self->mutex_);
            if (!self->is_current_pc_generation(generation)) {
                return;
            }
            callback = self->audio_track_cb_;
            user_data = self->audio_track_ud_;
        }
        if (callback) {
            callback(
                user_data,
                sender_id.c_str(),
                pkt.data(),
                static_cast<int>(pkt.size())
            );
        }
    });

    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (!is_current_pc_generation(generation)) {
            return;
        }
        incoming_tracks_.push_back(track);
        recv_track_count_.fetch_add(1, std::memory_order_relaxed);
    }
}

void PeerConnectionImpl::setup_dc_handlers(
    std::shared_ptr<rtc::DataChannel> dc,
    bool reliable,
    uint64_t generation
) {
    const std::weak_ptr<PeerConnectionImpl> weak_self = weak_from_this();
    dc->onOpen([weak_self, reliable, generation]() {
        const auto self = weak_self.lock();
        if (!self) {
            return;
        }
        std::lock_guard<std::mutex> lock(self->mutex_);
        if (!self->is_current_pc_generation(generation)) {
            return;
        }
        auto& open_flag = reliable
            ? self->reliable_open_
            : self->unreliable_open_;
        open_flag.store(true, std::memory_order_release);
    });
    dc->onClosed([weak_self, reliable, generation]() {
        const auto self = weak_self.lock();
        if (!self) {
            return;
        }
        std::lock_guard<std::mutex> lock(self->mutex_);
        if (!self->is_current_pc_generation(generation)) {
            return;
        }
        auto& open_flag = reliable
            ? self->reliable_open_
            : self->unreliable_open_;
        open_flag.store(false, std::memory_order_release);
    });
    dc->onError([weak_self, reliable, generation](std::string) {
        const auto self = weak_self.lock();
        if (!self) {
            return;
        }
        std::lock_guard<std::mutex> lock(self->mutex_);
        if (!self->is_current_pc_generation(generation)) {
            return;
        }
        auto& open_flag = reliable
            ? self->reliable_open_
            : self->unreliable_open_;
        open_flag.store(false, std::memory_order_release);
    });

    dc->onMessage([weak_self, reliable, generation](auto data) {
        const auto self = weak_self.lock();
        if (!self || !self->is_current_pc_generation(generation)) {
            return;
        }
        if (auto* str = std::get_if<std::string>(&data)) {
            if (reliable && str->size() > 14 && str->substr(0, 14) == R"({"type":"pong")") {
                const auto pos = str->find("\"ts\":");
                if (pos != std::string::npos) {
                    const int64_t sent_ts = std::strtoll(str->c_str() + pos + 5, nullptr, 10);
                    const auto now = std::chrono::steady_clock::now();
                    const int64_t now_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                        now.time_since_epoch()
                    ).count();
                    self->last_pong_ts_ms_.store(now_ms, std::memory_order_relaxed);
                    const float rtt = static_cast<float>(now_ms - sent_ts);
                    if (rtt >= 0 && rtt < 10000) {
                        const float prev =
                            self->rtt_ms_.load(std::memory_order_relaxed);
                        const float smoothed =
                            (prev < 0.1f) ? rtt : prev * 0.7f + rtt * 0.3f;
                        self->rtt_ms_.store(smoothed, std::memory_order_relaxed);
                    }
                }
            }
            return;
        }
        if (auto* bin = std::get_if<rtc::binary>(&data)) {
            const auto* bytes = reinterpret_cast<const uint8_t*>(bin->data());
            const auto size = static_cast<int>(bin->size());

            if (reliable && size > 14
                && std::memcmp(bytes, R"({"type":"pong")", 14) == 0) {
                const std::string pong(
                    reinterpret_cast<const char*>(bytes),
                    static_cast<size_t>(size)
                );
                const auto pos = pong.find("\"ts\":");
                if (pos != std::string::npos) {
                    const int64_t sent_ts = std::strtoll(pong.c_str() + pos + 5, nullptr, 10);
                    const auto now = std::chrono::steady_clock::now();
                    const int64_t now_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                        now.time_since_epoch()
                    ).count();
                    self->last_pong_ts_ms_.store(now_ms, std::memory_order_relaxed);
                    const float rtt = static_cast<float>(now_ms - sent_ts);
                    if (rtt >= 0 && rtt < 10000) {
                        const float prev =
                            self->rtt_ms_.load(std::memory_order_relaxed);
                        const float smoothed =
                            (prev < 0.1f) ? rtt : prev * 0.7f + rtt * 0.3f;
                        self->rtt_ms_.store(smoothed, std::memory_order_relaxed);
                    }
                }
            }

            if (!reliable) {
                std::lock_guard<std::mutex> lock(self->recv_mutex_);
                if (self->recv_queue_.size() < MAX_RECV_QUEUE) {
                    self->recv_queue_.emplace(bytes, bytes + size);
                }
            }
            MelloPeerDataCallback callback = nullptr;
            void* user_data = nullptr;
            {
                std::lock_guard<std::mutex> lock(self->mutex_);
                if (!self->is_current_pc_generation(generation)) {
                    return;
                }
                callback = self->data_cb_;
                user_data = self->data_ud_;
            }
            if (callback) {
                callback(user_data, bytes, size, reliable);
            }
        }
    });
}

void PeerConnectionImpl::setup_voice_channels() {
    const uint64_t generation =
        pc_generation_.load(std::memory_order_acquire);
#if RTC_ENABLE_MEDIA
    rtc::Description::Audio audio("audio", rtc::Description::Direction::SendRecv);
    audio.addOpusCodec(111, "minptime=10;useinbandfec=1");
    auto audio_track = pc_->addTrack(audio);

    const auto ssrc = generate_ssrc();
    const auto rtpConfig = std::make_shared<rtc::RtpPacketizationConfig>(
        ssrc, "mello", 111, rtc::OpusRtpPacketizer::DefaultClockRate);
    const auto packetizer = std::make_shared<rtc::OpusRtpPacketizer>(rtpConfig);
    packetizer->addToChain(std::make_shared<rtc::RtcpSrReporter>(rtpConfig));
    audio_track->setMediaHandler(packetizer);
    {
        std::lock_guard<std::mutex> lock(mutex_);
        audio_track_ = audio_track;
    }
#endif

    rtc::DataChannelInit dcInit;
    dcInit.reliability.unordered = true;
    dcInit.reliability.maxRetransmits = 0;
    auto unreliable = pc_->createDataChannel("audio", dcInit);
    {
        std::lock_guard<std::mutex> lock(mutex_);
        unreliable_dc_ = unreliable;
    }
    setup_dc_handlers(unreliable, false, generation);

    auto reliable = pc_->createDataChannel("control");
    {
        std::lock_guard<std::mutex> lock(mutex_);
        reliable_dc_ = reliable;
    }
    setup_dc_handlers(reliable, true, generation);
}

void PeerConnectionImpl::setup_stream_offer_channels() {
    const uint64_t generation =
        pc_generation_.load(std::memory_order_acquire);
    const auto direction = (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_HOST)
        ? rtc::Description::Direction::SendOnly
        : rtc::Description::Direction::RecvOnly;

    if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_HOST) {
        video_ssrc_ = generate_ssrc();
        video_cname_ = generate_cname();
    }

    auto video = make_stream_video_description(direction, kVideoMid);
    auto video_track = pc_->addTrack(std::move(video));
    {
        std::lock_guard<std::mutex> lock(mutex_);
        video_track_ = video_track;
    }
    wire_video_track_callbacks(generation);

    auto reliable = pc_->createDataChannel("control");
    {
        std::lock_guard<std::mutex> lock(mutex_);
        reliable_dc_ = reliable;
    }
    setup_dc_handlers(reliable, true, generation);
}

void PeerConnectionImpl::setup_control_dc_answer_handlers() {
    const uint64_t generation =
        pc_generation_.load(std::memory_order_acquire);
    const std::weak_ptr<PeerConnectionImpl> weak_self = weak_from_this();
    pc_->onDataChannel(
        [weak_self, generation](std::shared_ptr<rtc::DataChannel> dc) {
        const auto self = weak_self.lock();
        if (!self || !self->is_current_pc_generation(generation)) {
            return;
        }
        const auto label = dc->label();
        if (label == "control") {
            {
                std::lock_guard<std::mutex> lock(self->mutex_);
                if (!self->is_current_pc_generation(generation)) {
                    return;
                }
                self->reliable_dc_ = dc;
            }
            self->setup_dc_handlers(dc, true, generation);
        }
    });
}

void PeerConnectionImpl::begin_local_sdp_wait(uint64_t generation) {
    std::lock_guard<std::mutex> lock(sdp_mutex_);
    sdp_generation_ = generation;
    sdp_ready_ = false;
    local_sdp_.clear();
}

const char* PeerConnectionImpl::wait_for_local_sdp(uint64_t generation) {
    std::unique_lock<std::mutex> lock(sdp_mutex_);
    const bool ready = sdp_cv_.wait_for(
        lock,
        std::chrono::seconds(5),
        [this, generation] {
            return sdp_ready_ && sdp_generation_ == generation;
        }
    );
    return ready && !local_sdp_.empty() ? local_sdp_.c_str() : nullptr;
}

const char* PeerConnectionImpl::create_offer() {
    std::lock_guard<std::mutex> negotiation_lock(negotiation_mutex_);
    try {
        recreate_pc_locked();
        const uint64_t generation =
            pc_generation_.load(std::memory_order_acquire);
        begin_local_sdp_wait(generation);

        if (is_stream_role(role_)) {
            setup_stream_offer_channels();
        } else {
            setup_voice_channels();
        }

        std::shared_ptr<rtc::PeerConnection> pc;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            pc = pc_;
        }
        if (!pc) {
            return nullptr;
        }
        pc->setLocalDescription();
        return wait_for_local_sdp(generation);
    } catch (...) {
        close_pc_locked();
        return nullptr;
    }
}

const char* PeerConnectionImpl::create_answer(const char* offer_sdp) {
    std::lock_guard<std::mutex> negotiation_lock(negotiation_mutex_);
    try {
        rtc::Description offer(offer_sdp, rtc::Description::Type::Offer);
        std::optional<rtc::Description::Video> prepared_video;
        if (is_stream_role(role_)) {
            std::string error;
            prepared_video = prepare_video_for_answer(offer, error);
            if (!prepared_video) {
                return nullptr;
            }
        } else if (video_media_count(offer) != 0) {
            return nullptr;
        }

        recreate_pc_locked();
        const uint64_t generation =
            pc_generation_.load(std::memory_order_acquire);
        begin_local_sdp_wait(generation);

        if (is_stream_role(role_)) {
            setup_control_dc_answer_handlers();
            if (!replace_video_track_for_answer(std::move(*prepared_video))) {
                close_pc_locked();
                return nullptr;
            }
        } else {
            const std::weak_ptr<PeerConnectionImpl> weak_self =
                weak_from_this();
            pc_->onDataChannel(
                [weak_self, generation](std::shared_ptr<rtc::DataChannel> dc) {
                const auto self = weak_self.lock();
                if (!self || !self->is_current_pc_generation(generation)) {
                    return;
                }
                const auto label = dc->label();
                if (label == "audio") {
                    {
                        std::lock_guard<std::mutex> lock(self->mutex_);
                        if (!self->is_current_pc_generation(generation)) {
                            return;
                        }
                        self->unreliable_dc_ = dc;
                    }
                    self->setup_dc_handlers(dc, false, generation);
                } else if (label == "control") {
                    {
                        std::lock_guard<std::mutex> lock(self->mutex_);
                        if (!self->is_current_pc_generation(generation)) {
                            return;
                        }
                        self->reliable_dc_ = dc;
                    }
                    self->setup_dc_handlers(dc, true, generation);
                }
            });
        }

        std::shared_ptr<rtc::PeerConnection> pc;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            pc = pc_;
        }
        if (!pc) {
            return nullptr;
        }
        pc->setRemoteDescription(offer);
        return wait_for_local_sdp(generation);
    } catch (...) {
        close_pc_locked();
        return nullptr;
    }
}

const char* PeerConnectionImpl::handle_remote_offer(const char* sdp) {
    std::lock_guard<std::mutex> negotiation_lock(negotiation_mutex_);
    try {
        std::shared_ptr<rtc::PeerConnection> pc;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            pc = pc_;
        }
        if (!pc) {
            return nullptr;
        }

        rtc::Description offer(sdp, rtc::Description::Type::Offer);
        std::optional<rtc::Description::Video> prepared_video;
        if (is_stream_role(role_)) {
            std::string error;
            prepared_video = prepare_video_for_answer(offer, error);
            if (!prepared_video) {
                return nullptr;
            }
            std::shared_ptr<rtc::Track> existing;
            {
                std::lock_guard<std::mutex> lock(mutex_);
                existing = video_track_;
            }
            if (existing && !existing->isClosed()
                && existing->mid() != prepared_video->mid()) {
                return nullptr;
            }
        }

        const uint64_t generation =
            pc_generation_.load(std::memory_order_acquire);
        begin_local_sdp_wait(generation);
        if (prepared_video
            && !replace_video_track_for_answer(std::move(*prepared_video))) {
            return nullptr;
        }

        pc->setRemoteDescription(offer);
        const char* answer = wait_for_local_sdp(generation);
        try_start_video_pipeline(generation);
        return answer;
    } catch (const std::exception& error) {
        fprintf(
            stderr,
            "[mello-rtp] remote offer failed: %s\n",
            error.what()
        );
        try_start_video_pipeline();
        return nullptr;
    } catch (...) {
        try_start_video_pipeline();
        return nullptr;
    }
}

bool PeerConnectionImpl::set_remote_description(const char* sdp, bool is_offer) {
    std::lock_guard<std::mutex> negotiation_lock(negotiation_mutex_);
    try {
        const auto type = is_offer
            ? rtc::Description::Type::Offer
            : rtc::Description::Type::Answer;
        rtc::Description desc(sdp, type);
        std::shared_ptr<rtc::PeerConnection> pc;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            pc = pc_;
        }
        if (!pc) {
            return false;
        }
        pc->setRemoteDescription(desc);

        // TWCC capability drives the sender's stamping/estimator on the
        // stream video leg. Detected per remote description so renegotiation
        // and PC replacement stay honest.
        if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_HOST
            || role_ == MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER) {
            std::string error;
            if (const auto* video = find_single_video_media(desc, error)) {
                twcc_supported_ = remote_media_supports_twcc(*video);
            }
        }
        return true;
    } catch (...) {
        return false;
    }
}

bool PeerConnectionImpl::add_ice_candidate(
    const std::string& candidate,
    const std::string& mid,
    int /*mline_index*/
) {
    try {
        std::shared_ptr<rtc::PeerConnection> pc;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            pc = pc_;
        }
        if (!pc) {
            return false;
        }
        pc->addRemoteCandidate(rtc::Candidate(candidate, mid));
        return true;
    } catch (...) {
        return false;
    }
}

void PeerConnectionImpl::set_ice_callback(MelloIceCandidateCallback cb, void* user_data) {
    std::lock_guard<std::mutex> lock(mutex_);
    ice_cb_ = cb;
    ice_ud_ = user_data;
}

void PeerConnectionImpl::set_state_callback(MelloPeerStateCallback cb, void* user_data) {
    std::lock_guard<std::mutex> lock(mutex_);
    state_cb_ = cb;
    state_ud_ = user_data;
}

void PeerConnectionImpl::set_data_callback(MelloPeerDataCallback cb, void* user_data) {
    std::lock_guard<std::mutex> lock(mutex_);
    data_cb_ = cb;
    data_ud_ = user_data;
}

void PeerConnectionImpl::set_audio_track_callback(
    MelloAudioTrackCallback cb,
    void* user_data
) {
    std::lock_guard<std::mutex> lock(mutex_);
    audio_track_cb_ = cb;
    audio_track_ud_ = user_data;
}

bool PeerConnectionImpl::send_unreliable(const uint8_t* data, int size) {
    std::shared_ptr<rtc::DataChannel> channel;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        channel = unreliable_dc_;
    }
    if (!channel || !channel->isOpen()) {
        return false;
    }
    try {
        channel->send(
            reinterpret_cast<const std::byte*>(data),
            static_cast<size_t>(size)
        );
        return true;
    } catch (...) {
        return false;
    }
}

bool PeerConnectionImpl::send_reliable(const uint8_t* data, int size) {
    std::shared_ptr<rtc::DataChannel> channel;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        channel = reliable_dc_;
    }
    if (!channel || !channel->isOpen()) {
        return false;
    }
    try {
        channel->send(
            reinterpret_cast<const std::byte*>(data),
            static_cast<size_t>(size)
        );
        return true;
    } catch (...) {
        return false;
    }
}

bool PeerConnectionImpl::send_audio(const uint8_t* data, int size) {
    std::shared_ptr<rtc::Track> track;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        track = audio_track_;
    }
    const bool has_track = (track != nullptr);
    const bool open = has_track && track->isOpen();
    if (!open) {
        const auto count = send_audio_count_.fetch_add(1, std::memory_order_relaxed);
        if (count < 10 || (count % 500) == 0) {
            fprintf(stderr, "[mello-rtp] send_audio SKIP #%d: has_track=%d open=%d\n",
                    count + 1, has_track ? 1 : 0, open ? 1 : 0);
            fflush(stderr);
        }
        return false;
    }
    try {
        track->send(
            reinterpret_cast<const std::byte*>(data),
            static_cast<size_t>(size)
        );
        const int prev_skips = send_audio_count_.exchange(0, std::memory_order_relaxed);
        if (prev_skips > 0) {
            fprintf(stderr, "[mello-rtp] send_audio RECOVERED after %d skips\n", prev_skips);
            fflush(stderr);
        }
        return true;
    } catch (...) {
        return false;
    }
}

bool PeerConnectionImpl::is_connected() const {
    return connected_;
}

bool PeerConnectionImpl::is_unreliable_open() const {
    return unreliable_open_.load(std::memory_order_acquire);
}

bool PeerConnectionImpl::is_reliable_open() const {
    return reliable_open_.load(std::memory_order_acquire);
}

int PeerConnectionImpl::recv(uint8_t* buffer, int buffer_size) {
    std::lock_guard<std::mutex> lock(recv_mutex_);
    if (recv_queue_.empty()) {
        return 0;
    }

    auto& front = recv_queue_.front();
    const int copy_size = std::min(static_cast<int>(front.size()), buffer_size);
    std::memcpy(buffer, front.data(), copy_size);
    recv_queue_.pop();
    return copy_size;
}

void PeerConnectionImpl::send_ping() {
    std::shared_ptr<rtc::DataChannel> channel;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        channel = reliable_dc_;
    }
    if (!channel || !channel->isOpen()) {
        return;
    }
    const auto now = std::chrono::steady_clock::now();
    const int64_t ts = std::chrono::duration_cast<std::chrono::milliseconds>(
        now.time_since_epoch()
    ).count();
    const std::string msg = R"({"type":"ping","ts":)" + std::to_string(ts) + "}";
    try {
        channel->send(msg);
    } catch (...) {
    }
}

int64_t PeerConnectionImpl::pong_age_ms() const {
    const int64_t last_pong_ms = last_pong_ts_ms_.load(std::memory_order_relaxed);
    if (last_pong_ms <= 0) {
        return -1;
    }
    const auto now = std::chrono::steady_clock::now();
    const int64_t now_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
        now.time_since_epoch()
    ).count();
    const int64_t age = now_ms - last_pong_ms;
    return age < 0 ? 0 : age;
}

bool PeerConnectionImpl::video_send_access_unit(
    const uint8_t* data,
    size_t size,
    uint64_t capture_ts_us
) noexcept {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!rtp_video_sender_) {
        return false;
    }
    return rtp_video_sender_->send_access_unit(data, size, capture_ts_us);
}

int PeerConnectionImpl::video_recv_access_unit(
    uint8_t* buffer,
    size_t capacity,
    MelloRtpVideoAccessUnitInfo* info
) {
    static_assert(
        RtpVideoReceiverSession::kMaxOutputBytes
            <= static_cast<size_t>(std::numeric_limits<int>::max()),
        "receiver output bound must fit the C ABI int result"
    );
    static_assert(
        RtpVideoReceiverSession::kMaxOutputBytes
            <= static_cast<size_t>(std::numeric_limits<uint32_t>::max()),
        "receiver output bound must fit MelloRtpVideoAccessUnitInfo::size"
    );
    if (info) {
        std::memset(info, 0, sizeof(*info));
    }

    std::lock_guard<std::mutex> lock(mutex_);
    if (!rtp_video_receiver_) {
        return 0;
    }

    size_t size = 0;
    bool is_idr = false;
    uint32_t rtp_timestamp = 0;
    const auto result = rtp_video_receiver_->pop_access_unit(
        buffer,
        capacity,
        size,
        is_idr,
        rtp_timestamp
    );

    if (size > RtpVideoReceiverSession::kMaxOutputBytes
        || size > static_cast<size_t>(std::numeric_limits<int>::max())
        || size > static_cast<size_t>(std::numeric_limits<uint32_t>::max())) {
        return std::numeric_limits<int>::min();
    }

    if (info) {
        info->size = static_cast<uint32_t>(size);
        info->is_idr = is_idr ? 1 : 0;
        info->rtp_timestamp = rtp_timestamp;
        info->capture_timestamp_us = 0;
    }

    switch (result) {
    case RtpVideoReceiverPopResult::Empty:
        return 0;
    case RtpVideoReceiverPopResult::Ok:
        return static_cast<int>(size);
    case RtpVideoReceiverPopResult::BufferTooSmall:
        return -static_cast<int>(size);
    default:
        return std::numeric_limits<int>::min();
    }
}

bool PeerConnectionImpl::video_take_feedback(MelloPeerVideoFeedback* feedback) noexcept {
    if (!feedback) {
        return false;
    }

    try {
        std::lock_guard<std::mutex> lock(feedback_mutex_);
        if (feedback_queue_.empty()) {
            return false;
        }

        const QueuedVideoFeedback queued = feedback_queue_.front();
        feedback_queue_.pop_front();

        switch (queued.type) {
        case VideoFeedbackType::Pli:
            feedback->type = MELLO_PEER_VIDEO_FEEDBACK_PLI;
            break;
        case VideoFeedbackType::Remb:
            feedback->type = MELLO_PEER_VIDEO_FEEDBACK_REMB;
            break;
        case VideoFeedbackType::LocalIdrNeeded:
            feedback->type = MELLO_PEER_VIDEO_FEEDBACK_LOCAL_IDR_NEEDED;
            break;
        case VideoFeedbackType::GccTarget:
            feedback->type = MELLO_PEER_VIDEO_FEEDBACK_GCC_TARGET;
            break;
        default:
            feedback->type = MELLO_PEER_VIDEO_FEEDBACK_PLI;
            break;
        }
        feedback->remb_bitrate_bps = queued.remb_bitrate_bps;
        return true;
    } catch (...) {
        return false;
    }
}

bool PeerConnectionImpl::video_set_pacing_target(uint64_t bps) noexcept {
    std::lock_guard<std::mutex> lock(mutex_);
    pacing_target_bps_ = bps;
    if (!rtp_video_sender_) {
        return bps != 0;
    }
    return rtp_video_sender_->set_pacing_target_bps(bps);
}

bool PeerConnectionImpl::video_set_receive_target(uint32_t bps) noexcept {
    std::lock_guard<std::mutex> lock(mutex_);
    receive_target_bps_ = bps;
    if (!rtp_video_receiver_) {
        return bps != 0;
    }
    return rtp_video_receiver_->set_receive_target(bps);
}

void PeerConnectionImpl::video_get_stats(MelloRtpVideoStats* stats) const noexcept {
    if (!stats) {
        return;
    }

    std::lock_guard<std::mutex> lock(mutex_);
    std::memset(stats, 0, sizeof(*stats));
    stats->media_role = static_cast<uint8_t>(role_);
    stats->video_open =
        ((rtp_video_sender_ && rtp_video_sender_->is_open())
         || (rtp_video_receiver_ && rtp_video_receiver_->is_open()))
        ? 1
        : 0;

    if (rtp_video_sender_) {
        const auto tx = rtp_video_sender_->stats();
        stats->tx_access_units_enqueued = tx.access_units_enqueued;
        stats->tx_access_units_sent = tx.access_units_sent;
        stats->tx_access_units_dropped = tx.access_units_dropped;
        stats->tx_access_units_rejected = tx.access_units_rejected;
        stats->tx_bytes_sent = tx.bytes_sent;
        stats->tx_send_failures = tx.send_failures;
        stats->tx_rtp_packets_sent = tx.rtp_packets_sent;
        stats->tx_rtp_wire_bytes_sent = tx.rtp_wire_bytes_sent;
        stats->tx_queued_access_units = tx.queued_access_units;
        stats->tx_peak_queued_access_units = tx.peak_queued_access_units;
        stats->tx_queued_bytes = tx.queued_bytes;
        stats->tx_peak_queued_bytes = tx.peak_queued_bytes;
        stats->tx_pacing_target_bps = tx.pacing_target_bps;
        stats->tx_current_pacing_delay_us = tx.current_pacing_delay_us;
        stats->tx_max_pacing_delay_us = tx.max_pacing_delay_us;
        stats->tx_local_idr_requests = tx.local_idr_requests;
        stats->tx_pli_requests = tx.pli_requests;
        stats->tx_remb_reports = tx.remb_reports;
        stats->tx_latest_remb_bitrate_bps = tx.latest_remb_bitrate_bps;
        stats->tx_rtx_requests = tx.rtx_requests;
        stats->tx_rtx_sent = tx.rtx_sent;
        stats->tx_rtx_cache_misses = tx.rtx_cache_misses;
        stats->tx_rtx_queue_dropped = tx.rtx_queue_dropped;
        stats->tx_twcc_reports = tx.twcc_reports;
        stats->tx_gcc_target_bps = tx.gcc_target_bps;
        stats->tx_active = 1;
    }

    if (rtp_video_receiver_) {
        const auto rx = rtp_video_receiver_->stats();
        stats->rx_ingress_packets = rx.ingress_packets;
        stats->rx_ingress_bytes = rx.ingress_bytes;
        stats->rx_ingress_dropped_packets = rx.ingress_dropped_packets;
        stats->rx_ingress_dropped_bytes = rx.ingress_dropped_bytes;
        stats->rx_ingress_overflows = rx.ingress_overflows;
        stats->rx_ingress_queued_packets = rx.ingress_queued_packets;
        stats->rx_ingress_queued_bytes = rx.ingress_queued_bytes;
        stats->rx_peak_ingress_queued_packets = rx.peak_ingress_queued_packets;
        stats->rx_peak_ingress_queued_bytes = rx.peak_ingress_queued_bytes;
        stats->rx_wrong_ssrc_packets_after_recovery =
            rx.wrong_ssrc_packets_after_recovery;
        stats->rx_access_units_queued_total = rx.access_units_queued_total;
        stats->rx_access_unit_bytes_queued_total =
            rx.access_unit_bytes_queued_total;
        stats->rx_access_units_dropped = rx.access_units_dropped;
        stats->rx_access_unit_bytes_dropped = rx.access_unit_bytes_dropped;
        stats->rx_output_queued_access_units = rx.output_queued_access_units;
        stats->rx_output_queued_bytes = rx.output_queued_bytes;
        stats->rx_peak_output_queued_access_units =
            rx.peak_output_queued_access_units;
        stats->rx_peak_output_queued_bytes = rx.peak_output_queued_bytes;
        stats->rx_nack_packets_sent = rx.nack_packets_sent;
        stats->rx_nack_sequences_sent = rx.nack_sequences_sent;
        stats->rx_pli_requests = rx.pli_requests;
        stats->rx_pli_packets_sent = rx.pli_packets_sent;
        stats->rx_remb_packets_sent = rx.remb_packets_sent;
        stats->rx_twcc_packets_sent = rx.twcc_packets_sent;
        stats->rx_receiver_reports_sent = rx.receiver_reports_sent;
        stats->rx_sender_reports_received = rx.sender_reports_received;
        stats->rx_invalid_rtcp_packets = rx.invalid_rtcp_packets;
        stats->rx_feedback_send_failures = rx.feedback_send_failures;
        stats->rx_core_restarts = rx.core_restarts;
        stats->rx_payload_type = static_cast<uint32_t>(rx.payload_type);
        stats->rx_local_feedback_ssrc = rx.local_feedback_ssrc;
        stats->rx_remote_media_ssrc = rx.remote_media_ssrc;
        stats->rx_receive_target_bps = rx.receive_target_bps;
        stats->rx_has_remote_media_ssrc = rx.has_remote_media_ssrc ? 1 : 0;
        stats->rx_awaiting_output_idr = rx.awaiting_output_idr ? 1 : 0;

        const auto& core = rx.core;
        stats->rx_core_packets = core.packets;
        stats->rx_core_bytes_received = core.bytes_received;
        stats->rx_core_accepted_packets = core.accepted_packets;
        stats->rx_core_accepted_bytes = core.accepted_bytes;
        stats->rx_core_duplicates = core.duplicates;
        stats->rx_core_late_packets = core.late_packets;
        stats->rx_core_invalid_rtp_packets = core.invalid_rtp_packets;
        stats->rx_core_invalid_h264_packets = core.invalid_h264_packets;
        stats->rx_core_wrong_payload_type_packets =
            core.wrong_payload_type_packets;
        stats->rx_core_wrong_ssrc_packets = core.wrong_ssrc_packets;
        stats->rx_core_backwards_time_inputs = core.backwards_time_inputs;
        stats->rx_core_missing_sequences_detected =
            core.missing_sequences_detected;
        stats->rx_core_repaired_packets = core.repaired_packets;
        stats->rx_core_nacks = core.nacks;
        stats->rx_core_nack_callbacks = core.nack_callbacks;
        stats->rx_core_complete_access_units = core.complete_access_units;
        stats->rx_core_incomplete_access_units = core.incomplete_access_units;
        stats->rx_core_emitted_access_units = core.emitted_access_units;
        stats->rx_core_pli_requests = core.pli_requests;
        stats->rx_core_gate_dropped_access_units =
            core.gate_dropped_access_units;
        stats->rx_core_gate_entries = core.gate_entries;
        stats->rx_core_gate_exits = core.gate_exits;
        stats->rx_core_buffer_evictions = core.buffer_evictions;
        stats->rx_core_sequence_discontinuities =
            core.sequence_discontinuities;
        stats->rx_core_buffered_access_units = core.buffered_access_units;
        stats->rx_core_buffered_packets = core.buffered_packets;
        stats->rx_core_buffered_bytes = core.buffered_bytes;
        stats->rx_core_peak_buffered_access_units =
            core.peak_buffered_access_units;
        stats->rx_core_peak_buffered_packets = core.peak_buffered_packets;
        stats->rx_core_peak_buffered_bytes = core.peak_buffered_bytes;
        stats->rx_core_has_ssrc = core.has_ssrc ? 1 : 0;
        stats->rx_core_ssrc = core.ssrc;
        stats->rx_core_extended_highest_sequence = core.extended_highest_sequence;
        stats->rx_core_cumulative_loss = static_cast<uint64_t>(
            core.cumulative_loss < 0 ? 0 : core.cumulative_loss
        );
        stats->rx_core_interarrival_jitter = core.interarrival_jitter;
        stats->rx_core_gated = core.gated ? 1 : 0;
        stats->rx_active = 1;
    }
}

bool PeerConnectionImpl::video_is_open() const noexcept {
    std::lock_guard<std::mutex> lock(mutex_);
    if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_HOST) {
        return rtp_video_sender_ && rtp_video_sender_->is_open();
    }
    if (role_ == MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER) {
        return rtp_video_receiver_ && rtp_video_receiver_->is_open();
    }
    return false;
}

} // namespace mello::transport
