#include <gtest/gtest.h>

#include "transport/rtp_h264_receiver.hpp"
#include "transport/rtp_video_receiver_session.hpp"
#include "transport/rtp_video_sender.hpp"
#include "transport/twcc.hpp"

#include <rtc/rtc.hpp>
#include <rtc/rtp.hpp>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstring>
#include <deque>
#include <future>
#include <initializer_list>
#include <mutex>
#include <optional>
#include <stdexcept>
#include <thread>
#include <unordered_set>
#include <utility>
#include <vector>

using mello::transport::RtpH264Receiver;
using mello::transport::RtpVideoSender;
using mello::transport::RtpVideoSenderConfig;
using mello::transport::RtpVideoSenderStats;
using mello::transport::SendAccessUnitResult;

namespace {

using namespace std::chrono_literals;

bool send_au_accepted(
    RtpVideoSender& sender,
    const uint8_t* data,
    size_t size,
    uint64_t capture_ts_us
) {
    return sender.send_access_unit(data, size, capture_ts_us)
        == SendAccessUnitResult::Accepted;
}

constexpr uint8_t kPayloadType = 96;
constexpr uint32_t kSenderSsrc = 0x12345678;
constexpr uint32_t kRtxSsrc = 0x0badf00d;
constexpr uint8_t kRtxPayloadType = 97;
constexpr char kCname[] = "mello-rtp-test";

bool is_h264_format(const std::string& format) noexcept {
    return format.size() == 4
        && (format[0] == 'H' || format[0] == 'h')
        && format[1] == '2'
        && format[2] == '6'
        && format[3] == '4';
}

std::vector<uint8_t> annex_b(std::initializer_list<std::vector<uint8_t>> nals) {
    std::vector<uint8_t> bytes;
    for (const auto& nal : nals) {
        bytes.insert(bytes.end(), {0, 0, 0, 1});
        bytes.insert(bytes.end(), nal.begin(), nal.end());
    }
    return bytes;
}

std::vector<uint8_t> make_idr_access_unit() {
    return annex_b({{0x67, 0x42, 0x00, 0x1f},
                    {0x68, 0xce, 0x38, 0x80},
                    {0x65, 0x88, 0x84, 0x00}});
}

std::vector<uint8_t> make_delta_access_unit(uint8_t suffix) {
    return annex_b({{0x61, suffix, 0x10, 0x20}});
}

std::vector<uint8_t> make_large_delta_access_unit(size_t nal_payload_bytes) {
    std::vector<uint8_t> nal(nal_payload_bytes, 0x55);
    nal[0] = 0x61;
    return annex_b({nal});
}

struct ParsedRtpPacket {
    uint32_t ssrc = 0;
    // The first two payload bytes (the original sequence of an RTX packet).
    uint16_t payload_prefix = 0;
    uint16_t sequence = 0;
    uint32_t timestamp = 0;
    bool marker = false;
    uint8_t payload_type = 0;
    size_t payload_size = 0;
    size_t wire_size = 0;
    std::chrono::steady_clock::time_point received_at{};
};

std::optional<ParsedRtpPacket> parse_rtp_packet(
    const uint8_t* data,
    size_t size,
    std::chrono::steady_clock::time_point received_at
) {
    if (data == nullptr || size < sizeof(rtc::RtpHeader)) {
        return std::nullopt;
    }

    const auto* const header = reinterpret_cast<const rtc::RtpHeader*>(data);
    if (header->version() != 2) {
        return std::nullopt;
    }

    const size_t header_size = header->getSize();
    if (header_size > size) {
        return std::nullopt;
    }

    ParsedRtpPacket packet;
    packet.ssrc = header->ssrc();
    size_t payload_at = 12 + static_cast<size_t>(data[0] & 0x0f) * 4;
    if ((data[0] & 0x10) != 0 && payload_at + 4 <= size) {
        payload_at += 4 + static_cast<size_t>((data[payload_at + 2] << 8) | data[payload_at + 3]) * 4;
    }
    if (payload_at + 2 <= size) {
        packet.payload_prefix = static_cast<uint16_t>((data[payload_at] << 8) | data[payload_at + 1]);
    }
    packet.sequence = header->seqNumber();
    packet.timestamp = header->timestamp();
    packet.marker = header->marker() != 0;
    packet.payload_type = header->payloadType();
    packet.payload_size = size - header_size;
    packet.wire_size = size;
    packet.received_at = received_at;
    return packet;
}

rtc::Configuration make_loopback_config() {
    rtc::Configuration config;
    config.iceServers.clear();
    config.forceMediaTransport = true;
    // Loopback, OS-chosen ports. A fixed range from 50000 fails on a Windows
    // machine where Hyper-V or WSL reserved those ports: gathering throws
    // "Failed to gather local ICE candidates". See
    // PeerConnectionImpl::apply_loopback_ice_config.
    config.bindAddress = "127.0.0.1";
    return config;
}

void wire_signaling(
    const std::shared_ptr<rtc::PeerConnection>& local,
    const std::shared_ptr<rtc::PeerConnection>& remote
) {
    local->onLocalDescription([remote](rtc::Description description) {
        remote->setRemoteDescription(std::move(description));
    });
    local->onLocalCandidate([remote](rtc::Candidate candidate) {
        remote->addRemoteCandidate(std::move(candidate));
    });
}

template <typename Predicate>
bool wait_until(
    Predicate predicate,
    std::chrono::milliseconds timeout
) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (std::chrono::steady_clock::now() < deadline) {
        if (predicate()) {
            return true;
        }
        std::this_thread::sleep_for(5ms);
    }
    return predicate();
}

struct EmittedAccessUnit {
    std::vector<uint8_t> bytes;
    bool is_idr = false;
    uint32_t timestamp = 0;
};

class LoopbackVideoLink {
public:
    explicit LoopbackVideoLink(bool with_rtx = false) {
        const auto config = make_loopback_config();

        offerer_ = std::make_shared<rtc::PeerConnection>(config);
        answerer_ = std::make_shared<rtc::PeerConnection>(config);
        wire_signaling(offerer_, answerer_);
        wire_signaling(answerer_, offerer_);

        offerer_->onStateChange([this](rtc::PeerConnection::State state) {
            if (state == rtc::PeerConnection::State::Connected) {
                offerer_connected_.store(true, std::memory_order_release);
            }
        });
        answerer_->onStateChange([this](rtc::PeerConnection::State state) {
            if (state == rtc::PeerConnection::State::Connected) {
                answerer_connected_.store(true, std::memory_order_release);
            }
        });

        answerer_->onTrack([this](std::shared_ptr<rtc::Track> track) {
            std::lock_guard<std::mutex> lock(mutex_);
            receiver_track_ = std::move(track);
            install_receiver_handler();
            cv_.notify_all();
        });

        rtc::Description::Video video(
            "video",
            rtc::Description::Direction::SendOnly
        );
        video.addH264Codec(kPayloadType);
        video.addSSRC(kSenderSsrc, kCname, "mello-stream", "mello-video");
        if (with_rtx) {
            // Mirrors the host offer (PeerConnectionImpl::make_stream_video_description).
            video.addRtxCodec(kRtxPayloadType, kPayloadType, 90'000);
            video.addSSRC(kRtxSsrc, kCname, "mello-stream", "mello-video");
            video.addAttribute("ssrc-group:FID " + std::to_string(kSenderSsrc)
                               + " " + std::to_string(kRtxSsrc));
        }
        sender_track_ = offerer_->addTrack(std::move(video));

        sender_track_->onOpen([this]() {
            sender_track_open_.store(true, std::memory_order_release);
        });

        offerer_->setLocalDescription();

        // Wait on exactly what is asserted below. onTrack fires when the track is
        // negotiated, which is strictly earlier than the track being open, so
        // waiting on a "track arrived" flag and then requiring isOpen() leaves a
        // window where the wait succeeds and the next line fails. That window is
        // small — one observed failure landed 39 ms in — but it is real, and it
        // is the flake this harness kept producing.
        //
        // Reading receiver_track_ under the mutex also removes the unsynchronised
        // access: wait_until polls without holding it, while onTrack writes under it.
        if (!wait_until([this]() {
                if (!offerer_connected_.load(std::memory_order_acquire)
                    || !answerer_connected_.load(std::memory_order_acquire)
                    || !sender_track_open_.load(std::memory_order_acquire)) {
                    return false;
                }
                std::lock_guard<std::mutex> lock(mutex_);
                return receiver_track_ != nullptr && receiver_track_->isOpen();
            },
            10s)) {
            throw std::runtime_error("loopback PeerConnection did not connect");
        }

        if (!receiver_track_ || !receiver_track_->isOpen()) {
            throw std::runtime_error("receiver track did not open");
        }
        if (!sender_track_ || !sender_track_->isOpen()) {
            throw std::runtime_error("sender track did not open");
        }
    }

    ~LoopbackVideoLink() {
        try {
            if (receiver_track_) {
                receiver_track_->onMessage(nullptr);
            }
            if (sender_track_) {
                sender_track_->onMessage(nullptr);
            }
            if (offerer_) {
                offerer_->resetCallbacks();
                offerer_->close();
            }
            if (answerer_) {
                answerer_->resetCallbacks();
                answerer_->close();
            }
        } catch (...) {
        }
    }

    std::shared_ptr<rtc::Track> sender_track() const { return sender_track_; }
    std::shared_ptr<rtc::Track> receiver_track() const { return receiver_track_; }

    RtpVideoSender make_sender(
        uint64_t pacing_target_bps,
        RtpVideoSender::PliCallback on_pli = {},
        RtpVideoSender::RembCallback on_remb = {},
        RtpVideoSender::LocalIdrNeededCallback on_local_idr_needed = {},
        bool twcc_enabled = false,
        bool with_rtx = false
    ) {
        RtpVideoSenderConfig config;
        config.ssrc = kSenderSsrc;
        config.payload_type = kPayloadType;
        config.cname = kCname;
        config.pacing_target_bps = pacing_target_bps;
        config.twcc_enabled = twcc_enabled;
        if (with_rtx) {
            config.rtx_ssrc = kRtxSsrc;
            config.rtx_payload_type = kRtxPayloadType;
        }
        RtpVideoSender sender(
            sender_track_,
            config,
            std::move(on_pli),
            std::move(on_remb),
            std::move(on_local_idr_needed)
        );
        if (!sender.is_open()) {
            throw std::runtime_error("RtpVideoSender failed to attach");
        }
        return sender;
    }

    void drain_receiver() {
        std::deque<std::pair<std::vector<uint8_t>, RtpH264Receiver::TimePoint>>
            pending;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            pending.swap(ingress_queue_);
        }

        for (auto& entry : pending) {
            receiver_.on_rtp_packet(
                entry.first.data(),
                entry.first.size(),
                entry.second
            );
        }
        receiver_.tick(RtpH264Receiver::TimePoint::clock::now());
    }

    bool wait_for_emitted_count(
        size_t expected,
        std::chrono::milliseconds timeout
    ) {
        return wait_until(
            [this, expected]() {
                drain_receiver();
                std::lock_guard<std::mutex> lock(mutex_);
                return emitted_.size() >= expected;
            },
            timeout
        );
    }

    bool wait_for_captured_count(
        size_t expected,
        std::chrono::milliseconds timeout
    ) {
        return wait_until(
            [this, expected]() {
                drain_receiver();
                std::lock_guard<std::mutex> lock(mutex_);
                return captured_rtp_.size() >= expected;
            },
            timeout
        );
    }

    bool wait_for_stats(
        const std::function<bool(const RtpVideoSenderStats&)>& predicate,
        const RtpVideoSender& sender,
        std::chrono::milliseconds timeout
    ) {
        return wait_until(
            [&]() {
                drain_receiver();
                return predicate(sender.stats());
            },
            timeout
        );
    }

    std::vector<EmittedAccessUnit> emitted() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return emitted_;
    }

    bool wait_for_marker_packet(std::chrono::milliseconds timeout) {
        return wait_until(
            [this]() {
                drain_receiver();
                std::lock_guard<std::mutex> lock(mutex_);
                for (const auto& packet : captured_rtp_) {
                    if (packet.marker) {
                        return true;
                    }
                }
                return false;
            },
            timeout
        );
    }

    std::vector<ParsedRtpPacket> captured_rtp() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return captured_rtp_;
    }

    void clear_captured_rtp() {
        std::lock_guard<std::mutex> lock(mutex_);
        captured_rtp_.clear();
    }

    RtpH264Receiver& receiver() { return receiver_; }

private:
    void install_receiver_handler() {
        receiver_track_->onMessage([this](rtc::message_variant data) {
            const auto* const binary = std::get_if<rtc::binary>(&data);
            if (binary == nullptr || binary->empty()) {
                return;
            }
            if (rtc::IsRtcp(*binary)) {
                return;
            }

            const auto now = RtpH264Receiver::TimePoint::clock::now();
            std::vector<uint8_t> packet;
            packet.reserve(binary->size());
            for (const auto byte : *binary) {
                packet.push_back(static_cast<uint8_t>(byte));
            }

            std::lock_guard<std::mutex> lock(mutex_);
            if (const auto parsed = parse_rtp_packet(
                    packet.data(),
                    packet.size(),
                    now)) {
                captured_rtp_.push_back(*parsed);
            }
            ingress_queue_.emplace_back(std::move(packet), now);
            cv_.notify_all();
        });
    }

    std::shared_ptr<rtc::PeerConnection> offerer_;
    std::shared_ptr<rtc::PeerConnection> answerer_;
    std::shared_ptr<rtc::Track> sender_track_;
    std::shared_ptr<rtc::Track> receiver_track_;

    std::atomic<bool> offerer_connected_{false};
    std::atomic<bool> answerer_connected_{false};
    std::atomic<bool> sender_track_open_{false};

    mutable std::mutex mutex_;
    std::condition_variable cv_;
    std::deque<std::pair<std::vector<uint8_t>, RtpH264Receiver::TimePoint>>
        ingress_queue_;
    std::vector<ParsedRtpPacket> captured_rtp_;
    std::vector<EmittedAccessUnit> emitted_;

    RtpH264Receiver receiver_{RtpH264Receiver::Callbacks{
        [this](const std::vector<uint8_t>& bytes,
               bool is_idr,
               uint32_t timestamp) {
            std::lock_guard<std::mutex> lock(mutex_);
            emitted_.push_back({bytes, is_idr, timestamp});
            cv_.notify_all();
        },
        {},
        {},
    }};
};

TEST(RtpVideoSenderNegotiationTest, NegotiatesH264Pt96SendRecvDirections) {
    LoopbackVideoLink link;

    const auto sender_desc = link.sender_track()->description();
    ASSERT_EQ(sender_desc.type(), "video");
    EXPECT_EQ(
        sender_desc.direction(),
        rtc::Description::Direction::SendOnly
    );
    ASSERT_TRUE(sender_desc.hasPayloadType(kPayloadType));
    const auto* const sender_map = sender_desc.rtpMap(kPayloadType);
    ASSERT_NE(sender_map, nullptr);
    EXPECT_TRUE(is_h264_format(sender_map->format));
    EXPECT_EQ(sender_map->clockRate, 90'000);

    const auto receiver_desc = link.receiver_track()->description();
    ASSERT_EQ(receiver_desc.type(), "video");
    EXPECT_EQ(
        receiver_desc.direction(),
        rtc::Description::Direction::RecvOnly
    );
    ASSERT_TRUE(receiver_desc.hasPayloadType(kPayloadType));
    const auto* const receiver_map = receiver_desc.rtpMap(kPayloadType);
    ASSERT_NE(receiver_map, nullptr);
    EXPECT_TRUE(is_h264_format(receiver_map->format));
    EXPECT_EQ(receiver_map->clockRate, 90'000);
}

TEST(RtpVideoSenderPacketizationTest, FragmentsAccessUnitWithOneTimestampAndFinalMarker) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(8'000'000);

    const auto access_unit = make_large_delta_access_unit(24 * 1024);
    ASSERT_TRUE(send_au_accepted(sender,
        access_unit.data(),
        access_unit.size(),
        1'000
    ));
    ASSERT_TRUE(link.wait_for_marker_packet(10s));

    const auto all_packets = link.captured_rtp();
    ASSERT_GT(all_packets.size(), 1u);

    const uint32_t expected_timestamp = all_packets.front().timestamp;
    std::vector<ParsedRtpPacket> packets;
    packets.reserve(all_packets.size());
    for (const auto& packet : all_packets) {
        if (packet.timestamp == expected_timestamp) {
            packets.push_back(packet);
        }
    }
    ASSERT_GT(packets.size(), 1u);
    size_t marker_count = 0;
    for (const auto& packet : packets) {
        EXPECT_EQ(packet.payload_type, kPayloadType);
        EXPECT_LE(packet.payload_size, 1100u);
        EXPECT_EQ(packet.timestamp, expected_timestamp);
        if (packet.marker) {
            ++marker_count;
        }
    }
    EXPECT_EQ(marker_count, 1u);
    EXPECT_TRUE(packets.back().marker);
}

TEST(RtpVideoSenderEndToEndTest, ReceiverReconstructsAnnexBAccessUnits) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(8'000'000);

    const auto idr = make_idr_access_unit();
    const auto delta_a = make_delta_access_unit(0x11);
    const auto delta_b = make_delta_access_unit(0x22);

    ASSERT_TRUE(send_au_accepted(sender,idr.data(), idr.size(), 0));
    ASSERT_TRUE(send_au_accepted(sender,delta_a.data(), delta_a.size(), 33'333));
    ASSERT_TRUE(send_au_accepted(sender,delta_b.data(), delta_b.size(), 66'666));

    ASSERT_TRUE(link.wait_for_emitted_count(3, 10s));

    const auto emitted = link.emitted();
    ASSERT_EQ(emitted.size(), 3u);
    EXPECT_EQ(emitted[0].bytes, idr);
    EXPECT_TRUE(emitted[0].is_idr);
    EXPECT_EQ(emitted[1].bytes, delta_a);
    EXPECT_FALSE(emitted[1].is_idr);
    EXPECT_EQ(emitted[2].bytes, delta_b);
    EXPECT_FALSE(emitted[2].is_idr);
}

TEST(RtpVideoSenderTimestampTest, MapsCaptureClockTo90kRtpTimestamps) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(8'000'000);

    const auto idr = make_idr_access_unit();
    const auto delta = make_delta_access_unit(0x33);
    ASSERT_TRUE(send_au_accepted(sender,idr.data(), idr.size(), 1'000'000));
    ASSERT_TRUE(send_au_accepted(sender,delta.data(), delta.size(), 1'033'333));

    ASSERT_TRUE(link.wait_for_emitted_count(2, 10s));

    const auto emitted = link.emitted();
    ASSERT_EQ(emitted.size(), 2u);
    const int32_t timestamp_delta = static_cast<int32_t>(
        emitted[1].timestamp - emitted[0].timestamp
    );
    EXPECT_NEAR(timestamp_delta, 3'000, 2);
}

TEST(RtpVideoSenderPacingTest, AccessUnitFragmentsArePacedAcrossWireTime) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(200'000);

    const auto access_unit = make_large_delta_access_unit(32 * 1024);
    ASSERT_TRUE(send_au_accepted(sender,
        access_unit.data(),
        access_unit.size(),
        5'000'000
    ));

    ASSERT_TRUE(link.wait_for_captured_count(12, 15s));
    const auto packets = link.captured_rtp();
    ASSERT_GE(packets.size(), 12u);

    const auto spread = std::chrono::duration_cast<std::chrono::milliseconds>(
        packets[11].received_at - packets[0].received_at
    );
    // The pacer runs at 2.5 x 200 kbps = 500 kbps: the 40 ms burst window
    // (2.5 kB) lets two fragments go at once, then each ~1.15 kB fragment
    // takes ~18 ms, so packets 0 to 11 span ~180 ms. A whole-AU burst
    // finishes in under 20 ms.
    EXPECT_GE(spread.count(), 120);
    EXPECT_LE(spread.count(), 1'000);
}

TEST(RtpVideoSenderPacingTest, AggregateSendRateTracksPacingTarget) {
    LoopbackVideoLink link;
    constexpr uint64_t pacing_bps = 200'000;
    RtpVideoSender sender = link.make_sender(pacing_bps);

    const auto access_unit = make_large_delta_access_unit(32 * 1024);
    ASSERT_TRUE(send_au_accepted(sender,
        access_unit.data(),
        access_unit.size(),
        0
    ));

    // 32 KiB NAL = 30 fragments = ~34 kB on the wire: ~0.5 s at the pacing
    // rate of 2.5 x 200 kbps, less the 40 ms burst window.
    ASSERT_TRUE(link.wait_for_captured_count(29, 15s));
    const auto packets = link.captured_rtp();
    ASSERT_GE(packets.size(), 29u);

    const auto total = std::chrono::duration_cast<std::chrono::milliseconds>(
        packets[28].received_at - packets[0].received_at
    );
    EXPECT_GE(total.count(), 350);
    EXPECT_LE(total.count(), 2'000);
}

TEST(RtpVideoSenderPacingTest, NextAccessUnitStartsAfterOnePacketSlot) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(200'000);

    const auto first = make_large_delta_access_unit(32 * 1024);
    ASSERT_TRUE(send_au_accepted(sender,first.data(), first.size(), 0));
    ASSERT_TRUE(link.wait_for_captured_count(29, 15s));
    const auto first_last = link.captured_rtp().back().received_at;
    link.clear_captured_rtp();

    ASSERT_TRUE(send_au_accepted(sender,
        make_delta_access_unit(0x44).data(),
        make_delta_access_unit(0x44).size(),
        33'333
    ));
    ASSERT_TRUE(link.wait_for_captured_count(1, 15s));

    const auto gap = std::chrono::duration_cast<std::chrono::milliseconds>(
        link.captured_rtp().front().received_at - first_last
    );
    // The next AU must start after roughly one packet slot (~44 ms), not
    // after the previous AU's full wire time (the old AU-granular sleep).
    EXPECT_GE(gap.count(), 5);
    EXPECT_LE(gap.count(), 500);
}

TEST(RtpVideoSenderRetransmitTest, NackRepairIsPacedAndCounted) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(8'000'000);

    const auto idr = make_idr_access_unit();
    ASSERT_TRUE(send_au_accepted(sender,idr.data(), idr.size(), 0));
    for (int index = 0; index < 4; ++index) {
        const auto delta = make_delta_access_unit(static_cast<uint8_t>(index));
        ASSERT_TRUE(send_au_accepted(sender,
            delta.data(),
            delta.size(),
            static_cast<uint64_t>(index + 1) * 33'333
        ));
    }
    ASSERT_TRUE(link.wait_for_captured_count(5, 10s));

    const uint16_t lost_sequence = link.captured_rtp().front().sequence;

    // One cached sequence (repair must succeed) and one sequence far outside
    // the cache window (must be counted as a cache miss).
    const auto nack = mello::transport::detail::make_generic_nack_packet(
        0xfeedface,
        kSenderSsrc,
        {lost_sequence, static_cast<uint16_t>(lost_sequence + 5'000)}
    );
    rtc::binary nack_message;
    nack_message.reserve(nack.size());
    for (const uint8_t byte : nack) {
        nack_message.push_back(static_cast<std::byte>(byte));
    }
    ASSERT_TRUE(link.receiver_track()->send(std::move(nack_message)));

    ASSERT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) {
            return stats.rtx_sent >= 1 && stats.rtx_cache_misses >= 1;
        },
        sender,
        10s
    ));

    // Wire-level duplicate detection is not usable here: the loopback
    // receiver already holds the packet, so the retransmitted copy is a
    // genuine SRTP replay and is correctly dropped by the replay window.
    // (In production, NACKs cover packets the receiver never saw, so repairs
    // always pass SRTP replay protection.) Assert the sender-side contract.
    const auto stats = sender.stats();
    EXPECT_GE(stats.rtx_requests, 2u);
    EXPECT_GE(stats.rtx_sent, 1u);
    EXPECT_GE(stats.rtx_cache_misses, 1u);
    EXPECT_EQ(stats.rtx_queue_dropped, 0u);
    EXPECT_EQ(stats.send_failures, 0u);
}

TEST(RtpVideoSenderTwccTest, ViewerFeedbackDrivesGccEstimate) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(8'000'000, {}, {}, {}, true);

    mello::transport::RtpVideoReceiverSessionConfig rx_config;
    rx_config.payload_type = kPayloadType;
    rx_config.twcc_enabled = true;
    mello::transport::RtpVideoReceiverSession receiver(
        link.receiver_track(),
        rx_config
    );
    ASSERT_TRUE(receiver.is_open());

    // Stream ~2s of AUs at 60fps: TWCC-stamped packets must still assemble
    // into complete AUs, and feedback must drive the sender's estimator.
    size_t popped = 0;
    for (int index = 0; index < 120; ++index) {
        const auto au = index == 0
            ? make_idr_access_unit()
            : make_delta_access_unit(static_cast<uint8_t>(index));
        ASSERT_TRUE(send_au_accepted(sender,
            au.data(),
            au.size(),
            static_cast<uint64_t>(index) * 33'333
        ));
        while (auto unit = receiver.pop_access_unit()) {
            ++popped;
        }
        std::this_thread::sleep_for(16ms);
    }
    // Drain whatever the session assembled during the final iterations.
    for (int drain = 0; drain < 20; ++drain) {
        while (auto unit = receiver.pop_access_unit()) {
            ++popped;
        }
        std::this_thread::sleep_for(5ms);
    }

    const auto sender_stats = sender.stats();
    EXPECT_GT(sender_stats.twcc_reports, 0u);
    EXPECT_GT(sender_stats.gcc_target_bps, 0u);
    const auto rx_stats = receiver.stats();
    EXPECT_GT(rx_stats.twcc_packets_sent, 0u);
    EXPECT_GT(popped, 30u);
}

std::vector<uint8_t> make_large_idr_access_unit(size_t idr_payload_bytes) {
    std::vector<uint8_t> idr(idr_payload_bytes, 0x5a);
    idr[0] = 0x65;
    return annex_b({{0x67, 0x42, 0x00, 0x1f}, {0x68, 0xce, 0x38, 0x80}, idr});
}

bool send_rtcp(const std::shared_ptr<rtc::Track>& track, const std::vector<uint8_t>& bytes) {
    rtc::binary message;
    message.reserve(bytes.size());
    for (const uint8_t byte : bytes) {
        message.push_back(static_cast<std::byte>(byte));
    }
    return track->send(std::move(message));
}

std::vector<uint8_t> make_pli_packet() {
    return {0x81, 206, 0x00, 0x02,
            0xfe, 0xed, 0xfa, 0xce,
            static_cast<uint8_t>(kSenderSsrc >> 24), static_cast<uint8_t>((kSenderSsrc >> 16) & 0xff),
            static_cast<uint8_t>((kSenderSsrc >> 8) & 0xff), static_cast<uint8_t>(kSenderSsrc & 0xff)};
}

std::vector<uint8_t> make_nack_packet(std::initializer_list<uint16_t> sequences) {
    return mello::transport::detail::make_generic_nack_packet(
        0xfeedface, kSenderSsrc, std::vector<uint16_t>(sequences));
}

// libwebrtc's protection bitrate: the encoder target is the goog_cc target
// less the retransmit rate of the last 500 ms, so repairs take the place of
// media under a cap instead of adding to it. The old sender gave the encoder
// the whole target: at the gate's cap change media and repairs together
// were twice the target.
TEST(RtpVideoSenderTwccTest, RetransmitsComeOutOfTheEncoderTarget) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(8'000'000, {}, {}, {}, true);
    uint16_t highest = 0;
    {
        mello::transport::RtpVideoReceiverSessionConfig rx_config;
        rx_config.payload_type = kPayloadType;
        rx_config.twcc_enabled = true;
        mello::transport::RtpVideoReceiverSession receiver(link.receiver_track(), rx_config);
        ASSERT_TRUE(receiver.is_open());

        // About 1 s of 10 kB frames: TWCC feedback sets the goog_cc target.
        // An access unit the bounded queue rejects does not matter here.
        for (int index = 0; index < 60; ++index) {
            const auto au = index == 0 ? make_large_idr_access_unit(10'000)
                                       : make_large_delta_access_unit(10'000);
            (void)send_au_accepted(sender, au.data(), au.size(),
                                   static_cast<uint64_t>(index) * 16'667);
            while (receiver.pop_access_unit()) {
            }
            std::this_thread::sleep_for(16ms);
        }
        ASSERT_TRUE(link.wait_for_stats(
            [](const RtpVideoSenderStats& stats) { return stats.encoder_target_bps > 0; },
            sender,
            5s
        ));
        highest = static_cast<uint16_t>(receiver.stats().core.extended_highest_sequence);
    }
    // The receiver session is gone (its handler takes the track's sends), so
    // the test sends the NACK. The sender keeps its goog_cc target for the
    // 1 s before the feedback timeout.
    const auto before = sender.stats();
    EXPECT_EQ(before.encoder_target_bps, before.gcc_target_bps);

    // A NACK for the last 300 packets. The retransmit limit (the target over
    // 500 ms) admits part of them; the encoder target falls by their rate.
    std::vector<uint16_t> sequences;
    for (uint16_t back = 300; back > 0; --back) {
        sequences.push_back(static_cast<uint16_t>(highest - back));
    }
    const auto nack = mello::transport::detail::make_generic_nack_packet(
        0xfeedface, kSenderSsrc, sequences);
    ASSERT_TRUE(send_rtcp(link.receiver_track(), nack));
    uint64_t lowest_share_permille = 1000;
    const auto deadline = std::chrono::steady_clock::now() + 500ms;
    while (std::chrono::steady_clock::now() < deadline) {
        const auto stats = sender.stats();
        if (stats.gcc_target_bps > 0) {
            lowest_share_permille = std::min<uint64_t>(
                lowest_share_permille, stats.encoder_target_bps * 1000 / stats.gcc_target_bps);
        }
        std::this_thread::sleep_for(5ms);
    }
    const auto after = sender.stats();
    std::printf("[protection] %llu retransmits; the encoder target fell to %llu permille of the goog_cc target\n",
                static_cast<unsigned long long>(after.rtx_sent),
                static_cast<unsigned long long>(lowest_share_permille));
    EXPECT_GE(after.rtx_sent, 100u);
    EXPECT_LE(lowest_share_permille, 700u);
    EXPECT_GE(lowest_share_permille, 245u);  // the floor: a quarter of the target
}

// Without TWCC (no goog_cc) the pacer runs at 2.5 x the manager ceiling,
// libwebrtc's pacing factor. The old pacer ran at the ceiling (1.08 x the
// encoder bitrate) and dropped 17 to 20 % of the access units on a clean
// 1080p60 link (streaming-quality handoff section 3.3 finding 1).
TEST(RtpVideoSenderPacingTest, ThePacerRunsAtTwoAndAHalfTimesTheCeiling) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(400'000);

    // 64 KiB: 60 fragments, ~69 kB on the wire. At 1 Mbps (2.5 x 400 kbps)
    // that is ~550 ms less the 40 ms burst window; at 400 kbps it is 1.4 s.
    const auto access_unit = make_large_delta_access_unit(64 * 1024);
    ASSERT_TRUE(send_au_accepted(sender, access_unit.data(), access_unit.size(), 0));
    ASSERT_TRUE(link.wait_for_captured_count(60, 15s));
    const auto packets = link.captured_rtp();
    const auto spread = std::chrono::duration_cast<std::chrono::milliseconds>(
        packets[59].received_at - packets[0].received_at
    );
    EXPECT_GE(spread.count(), 350);
    EXPECT_LE(spread.count(), 900);
    EXPECT_EQ(sender.stats().pacing_rate_bps, 1'000'000u);
}

// The stage 4 gate: "an IDR leaves the host in under 50 ms" at the preset
// bitrate. The 1080p60 preset is 8 Mbps, so the ceiling is 8.64 Mbps and the
// pacer runs at 21.6 Mbps; the 40 ms burst window sends 108 kB at once. A
// 130 kB IDR is on the wire in about 10 ms. The old pacer (8.64 Mbps, no
// burst) took about 120 ms.
TEST(RtpVideoSenderPacingTest, AnIdrLeavesInUnder50MsAtThePresetBitrate) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(8'640'000);

    const auto idr = make_large_idr_access_unit(130'000);
    ASSERT_TRUE(send_au_accepted(sender, idr.data(), idr.size(), 0));
    ASSERT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) { return stats.idr_access_units_sent >= 1; },
        sender,
        10s
    ));
    const auto stats = sender.stats();
    EXPECT_LT(stats.last_idr_send_us, 50'000u);
    ASSERT_TRUE(link.wait_for_captured_count(110, 10s));
    const auto packets = link.captured_rtp();
    const auto spread = std::chrono::duration_cast<std::chrono::milliseconds>(
        packets.back().received_at - packets.front().received_at
    );
    EXPECT_LT(spread.count(), 50);
}

// Retransmits go out before the next media packet, inside an access unit.
// The old worker drained retransmits only between access units, so a repair
// waited for the whole frame in flight.
TEST(RtpVideoSenderRetransmitTest, RetransmitsInterleaveWithTheAccessUnitInFlight) {
    LoopbackVideoLink link;
    // 200 kbps ceiling: 500 kbps pacing, ~1.1 s for the 64 KiB access unit.
    RtpVideoSender sender = link.make_sender(200'000);

    const auto idr = make_idr_access_unit();
    ASSERT_TRUE(send_au_accepted(sender, idr.data(), idr.size(), 0));
    ASSERT_TRUE(link.wait_for_captured_count(3, 10s));
    const uint16_t cached = link.captured_rtp().front().sequence;

    const auto big = make_large_delta_access_unit(64 * 1024);
    ASSERT_TRUE(send_au_accepted(sender, big.data(), big.size(), 33'333));
    ASSERT_TRUE(link.wait_for_captured_count(6, 10s));
    ASSERT_TRUE(send_rtcp(link.receiver_track(), make_nack_packet({cached})));

    ASSERT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) { return stats.rtx_sent >= 1; },
        sender,
        5s
    ));
    // The big access unit is still on its way: the repair did not wait.
    EXPECT_EQ(sender.stats().access_units_sent, 1u);
}

// An IDR request drops the queued retransmits: they repair a picture the
// IDR replaces.
TEST(RtpVideoSenderRetransmitTest, APliDropsTheQueuedRetransmits) {
    LoopbackVideoLink link;
    // 8 kbps ceiling: 20 kbps pacing. The queued repairs cannot drain
    // before the PLI.
    RtpVideoSender sender = link.make_sender(8'000);

    const auto idr = make_idr_access_unit();
    ASSERT_TRUE(send_au_accepted(sender, idr.data(), idr.size(), 0));
    ASSERT_TRUE(link.wait_for_captured_count(3, 10s));
    const auto first = link.captured_rtp();
    const uint16_t a = first[0].sequence;
    const uint16_t b = first[1].sequence;
    const uint16_t c = first[2].sequence;
    // Hold the pacer on a large access unit, then ask for the same three
    // repairs ten times. One resend per round trip: three are queued.
    const auto big = make_large_delta_access_unit(16 * 1024);
    ASSERT_TRUE(send_au_accepted(sender, big.data(), big.size(), 33'333));
    for (int i = 0; i < 10; ++i) {
        ASSERT_TRUE(send_rtcp(link.receiver_track(), make_nack_packet({a, b, c})));
    }
    ASSERT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) { return stats.rtx_requests >= 30; },
        sender,
        5s
    ));
    ASSERT_TRUE(send_rtcp(link.receiver_track(), make_pli_packet()));
    ASSERT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) { return stats.rtx_queue_flushes >= 1; },
        sender,
        5s
    ));
    const auto stats = sender.stats();
    EXPECT_GE(stats.pli_requests, 1u);
    EXPECT_GE(stats.rtx_suppressed, 27u);
    EXPECT_GE(stats.rtx_queue_dropped, 2u);
    EXPECT_LE(stats.rtx_sent, 1u);
}

// libwebrtc's first retransmit rule: a packet is resent at most once per
// round trip. A receiver repeats its NACK until the repair arrives; the old
// sender resent the packet for every repeat, and at a cap change the
// repeats made 5 to 7 Mbps of retransmits into a 4.8 Mbps link (stage 4
// gate, hw congested).
TEST(RtpVideoSenderRetransmitTest, ARepeatedNackInsideOneRoundTripResendsOnce) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(8'000'000);

    const auto idr = make_idr_access_unit();
    ASSERT_TRUE(send_au_accepted(sender, idr.data(), idr.size(), 0));
    ASSERT_TRUE(link.wait_for_captured_count(3, 10s));
    const uint16_t lost = link.captured_rtp().front().sequence;
    // No TWCC here: the round trip is unknown, so the interval is 100 ms.
    for (int i = 0; i < 5; ++i) {
        ASSERT_TRUE(send_rtcp(link.receiver_track(), make_nack_packet({lost})));
    }
    ASSERT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) {
            return stats.rtx_requests >= 5 && stats.rtx_sent >= 1;
        },
        sender,
        5s
    ));
    const auto stats = sender.stats();
    EXPECT_EQ(stats.rtx_sent, 1u);
    EXPECT_EQ(stats.rtx_suppressed, 4u);
}

// libwebrtc's second retransmit rule: retransmits use at most the target
// rate over 500 ms. At a 400 kbps ceiling that is 25 kB: a NACK for all 55
// packets of a 64 KiB access unit resends about 20 of them.
TEST(RtpVideoSenderRetransmitTest, RetransmitsUseAtMostTheTargetRate) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(400'000);

    const auto big = make_large_delta_access_unit(64 * 1024);
    ASSERT_TRUE(send_au_accepted(sender, big.data(), big.size(), 0));
    ASSERT_TRUE(link.wait_for_captured_count(55, 10s));
    std::vector<uint16_t> sequences;
    for (const auto& packet : link.captured_rtp()) {
        sequences.push_back(packet.sequence);
    }
    const auto nack = mello::transport::detail::make_generic_nack_packet(
        0xfeedface, kSenderSsrc, sequences);
    ASSERT_TRUE(send_rtcp(link.receiver_track(), nack));
    ASSERT_TRUE(link.wait_for_stats(
        [&sequences](const RtpVideoSenderStats& stats) {
            return stats.rtx_requests >= sequences.size();
        },
        sender,
        5s
    ));
    const auto stats = sender.stats();
    const uint64_t admitted = stats.rtx_requests - stats.rtx_suppressed - stats.rtx_cache_misses;
    std::printf("[rtx-limit] %llu NACKed, %llu admitted, %llu suppressed\n",
                static_cast<unsigned long long>(stats.rtx_requests),
                static_cast<unsigned long long>(admitted),
                static_cast<unsigned long long>(stats.rtx_suppressed));
    EXPECT_EQ(stats.rtx_cache_misses, 0u);
    EXPECT_LE(admitted, 23u);
    EXPECT_GE(admitted, 15u);
}

// RFC 4588: with RTX negotiated a retransmit goes out on the RTX SSRC and
// payload type, with the original sequence number as the first two payload
// bytes. The old sender re-sent the packet on the media SSRC with its own
// sequence number, which a receiver cannot tell from a late original.
TEST(RtpVideoSenderRetransmitTest, ARetransmitGoesOutOnTheRtxSsrc) {
    LoopbackVideoLink link(true);
    RtpVideoSender sender = link.make_sender(8'000'000, {}, {}, {}, false, true);

    const auto idr = make_idr_access_unit();
    ASSERT_TRUE(send_au_accepted(sender, idr.data(), idr.size(), 0));
    ASSERT_TRUE(link.wait_for_captured_count(3, 10s));
    const uint16_t lost = link.captured_rtp().front().sequence;
    link.clear_captured_rtp();

    ASSERT_TRUE(send_rtcp(link.receiver_track(), make_nack_packet({lost})));
    ASSERT_TRUE(link.wait_for_captured_count(1, 5s));
    const auto packets = link.captured_rtp();
    EXPECT_EQ(packets.front().ssrc, kRtxSsrc);
    EXPECT_EQ(packets.front().payload_type, kRtxPayloadType);
    EXPECT_EQ(packets.front().payload_prefix, lost);
    EXPECT_GE(sender.stats().rtx_sent, 1u);
}

// The outage design of the goog_cc spike: after 1 s of sending without TWCC
// feedback, media stops (keepalive every 500 ms); the first feedback after
// it resumes, and the sender asks for an IDR. The old sender sent into the
// void for the whole outage.
TEST(RtpVideoSenderOutageTest, MediaStopsAfterOneSecondWithoutFeedbackAndResumes) {
    LoopbackVideoLink link;
    std::atomic<int> idr_requests{0};
    RtpVideoSender sender = link.make_sender(
        8'000'000, {}, {},
        [&idr_requests]() { idr_requests.fetch_add(1, std::memory_order_relaxed); },
        true);

    // 60 fps of small access units, and no receiver session: no feedback.
    int index = 0;
    const auto send_frame = [&]() {
        const auto au = index == 0 ? make_idr_access_unit()
                                   : make_delta_access_unit(static_cast<uint8_t>(index));
        (void)sender.send_access_unit(au.data(), au.size(), static_cast<uint64_t>(index) * 16'667);
        ++index;
    };
    const auto deadline = std::chrono::steady_clock::now() + 3s;
    while (std::chrono::steady_clock::now() < deadline && !sender.stats().send_paused) {
        send_frame();
        std::this_thread::sleep_for(16ms);
    }
    ASSERT_TRUE(sender.stats().send_paused);
    EXPECT_EQ(sender.stats().send_pauses, 1u);
    const uint64_t packets_at_pause = sender.stats().rtp_packets_sent;
    for (int i = 0; i < 30; ++i) {  // 0.5 s of frames while paused
        send_frame();
        std::this_thread::sleep_for(16ms);
    }
    // Keepalive only: one access unit of one packet per 500 ms.
    EXPECT_LE(sender.stats().rtp_packets_sent - packets_at_pause, 3u);

    mello::transport::TwccFeedbackGenerator generator;
    generator.on_packet(0, 5'000'000);
    ASSERT_TRUE(send_rtcp(link.receiver_track(), generator.build_feedback(0xfeedface, kSenderSsrc)));
    ASSERT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) { return !stats.send_paused; },
        sender,
        5s
    ));
    EXPECT_GE(idr_requests.load(std::memory_order_relaxed), 1);
}

TEST(RtpVideoSenderPacingTest, DynamicTargetUpdateIsObservedInStats) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(200'000);

    EXPECT_TRUE(sender.set_pacing_target_bps(1'500'000));
    EXPECT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) {
            return stats.pacing_target_bps == 1'500'000;
        },
        sender,
        2s
    ));
}

TEST(RtpVideoSenderAdmissionTest, QueueOverflowEntersIdrGate) {
    LoopbackVideoLink link;
    std::atomic<int> idr_requests{0};
    // Pace at 8 kbps so the queue cannot drain meaningfully while the burst
    // below is enqueued. Previously this used 20 Mbps and pushed exactly
    // kMaxQueuedAccessUnits tiny frames, so whether the queue ever reached its
    // bound depended on the sender racing the pacer — the test passed or failed
    // on scheduling luck (~25% failure rate in CI). Starving the pacer makes
    // overflow arithmetic rather than a race.
    RtpVideoSender sender = link.make_sender(
        8'000,
        {},
        {},
        [&idr_requests]() { idr_requests.fetch_add(1, std::memory_order_relaxed); }
    );

    const auto idr = make_idr_access_unit();
    ASSERT_TRUE(send_au_accepted(sender,idr.data(), idr.size(), 0));
    // The bound applies once the IDR has left: while it drains, deltas wait
    // behind it (AnOverflowWhileAnIdrDrainsAsksForNoSecondIdr).
    ASSERT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) { return stats.idr_access_units_sent >= 1; },
        sender,
        10s
    ));

    // Comfortably more than kMaxQueuedAccessUnits (16) so the bound is crossed
    // even if a frame or two does drain.
    for (int index = 0; index < 48; ++index) {
        const auto delta = make_delta_access_unit(static_cast<uint8_t>(index));
        (void)send_au_accepted(sender,
            delta.data(),
            delta.size(),
            static_cast<uint64_t>((index + 1) * 33'333)
        );
    }

    ASSERT_TRUE(link.wait_for_stats(
        [](const RtpVideoSenderStats& stats) {
            return stats.access_units_rejected > 0
                && stats.local_idr_requests > 0;
        },
        sender,
        10s
    ));

    const auto stats = sender.stats();
    EXPECT_GT(stats.access_units_rejected, 0u);
    EXPECT_GT(stats.local_idr_requests, 0u);
    EXPECT_LE(stats.peak_queued_access_units, RtpVideoSender::kMaxQueuedAccessUnits);
    EXPECT_LE(stats.peak_queued_bytes, RtpVideoSender::kMaxQueuedBytes);
    EXPECT_GE(idr_requests.load(std::memory_order_relaxed), 1);
}

// While an IDR drains, the deltas behind it wait: no overflow, no second
// IDR. The old sender dropped them at 16 access units and asked for another
// IDR, which then drained as slowly as the first: at the gate's cap change
// one PLI became three IDRs, each a burst into the full link.
TEST(RtpVideoSenderAdmissionTest, AnOverflowWhileAnIdrDrainsAsksForNoSecondIdr) {
    LoopbackVideoLink link;
    std::atomic<int> idr_requests{0};
    // 8 kbps ceiling: 20 kbps pacing. The 6 kB IDR takes about 2.4 s.
    RtpVideoSender sender = link.make_sender(
        8'000,
        {},
        {},
        [&idr_requests]() { idr_requests.fetch_add(1, std::memory_order_relaxed); }
    );
    const auto idr = make_large_idr_access_unit(6'000);
    ASSERT_TRUE(send_au_accepted(sender, idr.data(), idr.size(), 0));
    for (int index = 0; index < 40; ++index) {
        const auto delta = make_delta_access_unit(static_cast<uint8_t>(index));
        EXPECT_TRUE(send_au_accepted(sender, delta.data(), delta.size(),
                                     static_cast<uint64_t>(index + 1) * 16'667));
    }
    const auto stats = sender.stats();
    EXPECT_EQ(stats.idr_access_units_sent, 0u) << "the IDR must still drain";
    EXPECT_EQ(stats.access_units_rejected, 0u);
    EXPECT_EQ(stats.access_units_dropped, 0u);
    EXPECT_EQ(stats.local_idr_requests, 0u);
    EXPECT_EQ(idr_requests.load(std::memory_order_relaxed), 0);
    EXPECT_GT(stats.peak_queued_access_units, RtpVideoSender::kMaxQueuedAccessUnits);
}

TEST(RtpVideoSenderLifetimeTest, DestroysCleanlyUnderLoad) {
    auto completed = std::async(std::launch::async, []() -> bool {
        LoopbackVideoLink link;
        RtpVideoSender sender = link.make_sender(8'000'000);
        const auto idr = make_idr_access_unit();
        for (int index = 0; index < 6; ++index) {
            if (!send_au_accepted(sender,
                    idr.data(),
                    idr.size(),
                    static_cast<uint64_t>(index * 33'333))) {
                return false;
            }
        }
        return link.wait_for_captured_count(3, 10s);
    });

    ASSERT_EQ(completed.wait_for(20s), std::future_status::ready);
    EXPECT_TRUE(completed.get());
}

} // namespace
