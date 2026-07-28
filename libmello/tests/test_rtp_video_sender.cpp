#include <gtest/gtest.h>

#include "transport/rtp_h264_receiver.hpp"
#include "transport/rtp_video_receiver_session.hpp"
#include "transport/rtp_video_sender.hpp"
#include "transport/ulpfec.hpp"

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

namespace {

using namespace std::chrono_literals;

constexpr uint8_t kPayloadType = 96;
constexpr uint32_t kSenderSsrc = 0x12345678;
constexpr char kCname[] = "mello-rtp-test";

std::atomic<uint16_t> g_next_port_base{50000};

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
    config.bindAddress = "127.0.0.1";

    const uint16_t base = g_next_port_base.fetch_add(
        100,
        std::memory_order_relaxed
    );
    config.portRangeBegin = base;
    config.portRangeEnd = static_cast<uint16_t>(base + 99);
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

// Drops every `period`th ORIGINAL media RTP packet (retransmits, FEC on
// PT 127, and RTCP always pass; a retransmit reuses the original sequence
// number, so the sequence set keeps the drop pattern deterministic).
// Append at the END of a track's media-handler chain: incoming handlers run
// deepest-first, so drops land before the session consumes the messages.
class LossInjectingHandler final : public rtc::MediaHandler {
public:
    explicit LossInjectingHandler(size_t period) : period_(period) {}

    void incoming(
        rtc::message_vector& messages,
        const rtc::message_callback& /*send*/
    ) override {
        rtc::message_vector kept;
        kept.reserve(messages.size());
        for (auto& message : messages) {
            if (message && message->type == rtc::Message::Binary
                && message->size() >= 12) {
                const auto* const bytes =
                    static_cast<const std::byte*>(message->data());
                const uint8_t payload_type =
                    static_cast<uint8_t>(bytes[1]) & 0x7fU;
                if ((static_cast<uint8_t>(bytes[0]) >> 6) == 2
                    && payload_type != mello::transport::kUlpfecPayloadType) {
                    const uint16_t sequence =
                        static_cast<uint16_t>(
                            (static_cast<uint16_t>(bytes[2]) << 8)
                            | static_cast<uint16_t>(bytes[3])
                        );
                    if (seen_sequences_.insert(sequence).second
                        && ++original_seen_ % period_ == 0) {
                        ++dropped_;
                        continue;
                    }
                }
            }
            kept.push_back(std::move(message));
        }
        messages.swap(kept);
    }

    size_t dropped() const { return dropped_; }

private:
    const size_t period_;
    size_t original_seen_ = 0;
    size_t dropped_ = 0;
    std::unordered_set<uint16_t> seen_sequences_;
};

class LoopbackVideoLink {
public:
    LoopbackVideoLink() {
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
            receiver_track_ready_ = true;
            install_receiver_handler();
            cv_.notify_all();
        });

        rtc::Description::Video video(
            "video",
            rtc::Description::Direction::SendOnly
        );
        video.addH264Codec(kPayloadType);
        // Mirrors production SDP (PeerConnectionImpl::make_stream_video_description).
        video.addRtpMap(rtc::Description::Media::RtpMap(
            std::to_string(mello::transport::kUlpfecPayloadType)
            + " " + mello::transport::kUlpfecFormatName + "/90000"
        ));
        video.addSSRC(kSenderSsrc, kCname, "mello-stream", "mello-video");
        sender_track_ = offerer_->addTrack(std::move(video));

        sender_track_->onOpen([this]() {
            sender_track_open_.store(true, std::memory_order_release);
        });

        offerer_->setLocalDescription();

        if (!wait_until([this]() {
                return offerer_connected_.load(std::memory_order_acquire)
                    && answerer_connected_.load(std::memory_order_acquire)
                    && receiver_track_ready_
                    && sender_track_open_.load(std::memory_order_acquire);
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
        bool fec_enabled = false
    ) {
        RtpVideoSenderConfig config;
        config.ssrc = kSenderSsrc;
        config.payload_type = kPayloadType;
        config.cname = kCname;
        config.pacing_target_bps = pacing_target_bps;
        config.twcc_enabled = twcc_enabled;
        config.fec_enabled = fec_enabled;
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
    bool receiver_track_ready_ = false;

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
    ASSERT_TRUE(sender.send_access_unit(
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

    ASSERT_TRUE(sender.send_access_unit(idr.data(), idr.size(), 0));
    ASSERT_TRUE(sender.send_access_unit(delta_a.data(), delta_a.size(), 33'333));
    ASSERT_TRUE(sender.send_access_unit(delta_b.data(), delta_b.size(), 66'666));

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
    ASSERT_TRUE(sender.send_access_unit(idr.data(), idr.size(), 1'000'000));
    ASSERT_TRUE(sender.send_access_unit(delta.data(), delta.size(), 1'033'333));

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
    ASSERT_TRUE(sender.send_access_unit(
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
    // ~1100-byte fragments at 200 kbps occupy a ~44 ms slot each, so 11
    // slots span ~480 ms. The old whole-AU burst finished in <20 ms.
    EXPECT_GE(spread.count(), 200);
    EXPECT_LE(spread.count(), 1'500);
}

TEST(RtpVideoSenderPacingTest, AggregateSendRateTracksPacingTarget) {
    LoopbackVideoLink link;
    constexpr uint64_t pacing_bps = 200'000;
    RtpVideoSender sender = link.make_sender(pacing_bps);

    const auto access_unit = make_large_delta_access_unit(32 * 1024);
    ASSERT_TRUE(sender.send_access_unit(
        access_unit.data(),
        access_unit.size(),
        0
    ));

    // 32 KiB NAL ≈ 30 fragments ≈ 33 KB on the wire ≈ 1.3 s at 200 kbps.
    ASSERT_TRUE(link.wait_for_captured_count(29, 15s));
    const auto packets = link.captured_rtp();
    ASSERT_GE(packets.size(), 29u);

    const auto total = std::chrono::duration_cast<std::chrono::milliseconds>(
        packets[28].received_at - packets[0].received_at
    );
    EXPECT_GE(total.count(), 900);
    EXPECT_LE(total.count(), 4'000);
}

TEST(RtpVideoSenderPacingTest, NextAccessUnitStartsAfterOnePacketSlot) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(200'000);

    const auto first = make_large_delta_access_unit(32 * 1024);
    ASSERT_TRUE(sender.send_access_unit(first.data(), first.size(), 0));
    ASSERT_TRUE(link.wait_for_captured_count(29, 15s));
    const auto first_last = link.captured_rtp().back().received_at;
    link.clear_captured_rtp();

    ASSERT_TRUE(sender.send_access_unit(
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
    ASSERT_TRUE(sender.send_access_unit(idr.data(), idr.size(), 0));
    for (int index = 0; index < 4; ++index) {
        const auto delta = make_delta_access_unit(static_cast<uint8_t>(index));
        ASSERT_TRUE(sender.send_access_unit(
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
        ASSERT_TRUE(sender.send_access_unit(
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

TEST(RtpVideoSenderFecTest, ParityFecRepairsOneLossPerGroupWithoutPli) {
    LoopbackVideoLink link;
    RtpVideoSender sender = link.make_sender(200'000'000, {}, {}, {}, false, true);

    mello::transport::RtpVideoReceiverSessionConfig rx_config;
    rx_config.payload_type = kPayloadType;
    rx_config.fec_enabled = true;
    mello::transport::RtpVideoReceiverSession receiver(
        link.receiver_track(),
        rx_config
    );
    ASSERT_TRUE(receiver.is_open());

    // Drop every 11th media packet before the session's track: with groups
    // of 10 and drop period 11 (coprime), each parity group suffers exactly
    // one loss — parity-repairable.
    auto loss = std::make_shared<LossInjectingHandler>(11);
    link.receiver_track()->chainMediaHandler(loss);

    // 30 large AUs (~22 fragments each): a dropped fragment sits inside an
    // INCOMPLETE access unit, so the release floor stays behind it and the
    // parity repair lands within the AU deadline — the realistic FEC win.
    // (Single-packet AUs complete their neighbor instantly and advance the
    // release floor past any repair — by design, too-late frames are shed.)
    size_t popped = 0;
    std::vector<uint32_t> popped_timestamps;
    for (int index = 0; index < 30; ++index) {
        const auto au = index == 0
            ? make_idr_access_unit()
            : make_large_delta_access_unit(24 * 1024);
        if (!sender.send_access_unit(
                au.data(),
                au.size(),
                static_cast<uint64_t>(index) * 33'333
            )) {
            const auto debug_stats = sender.stats();
            fprintf(
                stderr,
                "send_access_unit failed at index %d: enqueued=%llu "
                "sent=%llu rejected=%llu dropped=%llu queued=%llu "
                "peakq=%llu failures=%llu rtp=%llu fec=%llu\n",
                index,
                (unsigned long long)debug_stats.access_units_enqueued,
                (unsigned long long)debug_stats.access_units_sent,
                (unsigned long long)debug_stats.access_units_rejected,
                (unsigned long long)debug_stats.access_units_dropped,
                (unsigned long long)debug_stats.queued_access_units,
                (unsigned long long)debug_stats.peak_queued_access_units,
                (unsigned long long)debug_stats.send_failures,
                (unsigned long long)debug_stats.rtp_packets_sent,
                (unsigned long long)debug_stats.fec_packets_sent
            );
            FAIL();
        }
        std::this_thread::sleep_for(2ms);
        while (auto unit = receiver.pop_access_unit()) {
            popped_timestamps.push_back(unit->rtp_timestamp);
            ++popped;
        }
    }

    const bool drained = wait_until(
        [&]() {
            while (auto unit = receiver.pop_access_unit()) {
                popped_timestamps.push_back(unit->rtp_timestamp);
                ++popped;
            }
            return popped >= 30;
        },
        10s
    );
    if (!drained) {
        std::sort(popped_timestamps.begin(), popped_timestamps.end());
        fprintf(stderr, "missing AU before timestamps:");
        for (size_t i = 1; i < popped_timestamps.size(); ++i) {
            if (popped_timestamps[i] - popped_timestamps[i - 1] > 3'500) {
                fprintf(stderr, " (after %u)", popped_timestamps[i - 1]);
            }
        }
        fprintf(stderr, "\n");
    }
    {
        const auto tx_debug = sender.stats();
        const auto rx_debug = receiver.stats();
        fprintf(
            stderr,
            "end: drained=%d popped=%zu dropped=%zu | tx fec=%llu rtp=%llu "
            "rtx_req=%llu rtx_sent=%llu | rx ingress=%llu fec_rec=%llu "
            "fec_unrec=%llu nack_seq=%llu core_acc=%llu core_missing=%llu "
            "core_repaired=%llu core_complete=%llu core_incomplete=%llu "
            "core_emitted=%llu aus_dropped=%llu pli=%llu restarts=%llu "
            "wrong_ssrc=%llu inv_rtp=%llu inv_h264=%llu dup=%llu late=%llu "
            "gate_entries=%llu gate_dropped=%llu\n",
            drained,
            popped,
            loss->dropped(),
            (unsigned long long)tx_debug.fec_packets_sent,
            (unsigned long long)tx_debug.rtp_packets_sent,
            (unsigned long long)tx_debug.rtx_requests,
            (unsigned long long)tx_debug.rtx_sent,
            (unsigned long long)rx_debug.ingress_packets,
            (unsigned long long)rx_debug.rx_fec_recovered,
            (unsigned long long)rx_debug.rx_fec_unrecoverable,
            (unsigned long long)rx_debug.nack_sequences_sent,
            (unsigned long long)rx_debug.core.accepted_packets,
            (unsigned long long)rx_debug.core.missing_sequences_detected,
            (unsigned long long)rx_debug.core.repaired_packets,
            (unsigned long long)rx_debug.core.complete_access_units,
            (unsigned long long)rx_debug.core.incomplete_access_units,
            (unsigned long long)rx_debug.core.emitted_access_units,
            (unsigned long long)rx_debug.access_units_dropped,
            (unsigned long long)rx_debug.pli_requests,
            (unsigned long long)rx_debug.core_restarts,
            (unsigned long long)rx_debug.wrong_ssrc_packets_after_recovery,
            (unsigned long long)rx_debug.core.invalid_rtp_packets,
            (unsigned long long)rx_debug.core.invalid_h264_packets,
            (unsigned long long)rx_debug.core.duplicates,
            (unsigned long long)rx_debug.core.late_packets,
            (unsigned long long)rx_debug.core.gate_entries,
            (unsigned long long)rx_debug.core.gate_dropped_access_units
        );
    }
    ASSERT_TRUE(drained);

    const auto tx = sender.stats();
    EXPECT_GE(tx.fec_packets_sent, 60u); // ~660 media packets = 66 groups of 10

    const auto rx = receiver.stats();
    EXPECT_EQ(popped, 30u);
    EXPECT_GT(rx.rx_fec_recovered, 0u);
    EXPECT_EQ(rx.pli_requests, 0u);
    EXPECT_EQ(rx.pli_packets_sent, 0u);
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
    RtpVideoSender sender = link.make_sender(
        20'000'000,
        {},
        {},
        [&idr_requests]() { idr_requests.fetch_add(1, std::memory_order_relaxed); }
    );

    const auto idr = make_idr_access_unit();
    ASSERT_TRUE(sender.send_access_unit(idr.data(), idr.size(), 0));

    for (int index = 0; index < 16; ++index) {
        const auto delta = make_delta_access_unit(static_cast<uint8_t>(index));
        (void)sender.send_access_unit(
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

TEST(RtpVideoSenderLifetimeTest, DestroysCleanlyUnderLoad) {
    auto completed = std::async(std::launch::async, []() -> bool {
        LoopbackVideoLink link;
        RtpVideoSender sender = link.make_sender(8'000'000);
        const auto idr = make_idr_access_unit();
        for (int index = 0; index < 6; ++index) {
            if (!sender.send_access_unit(
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
