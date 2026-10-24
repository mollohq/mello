// A/V sync of the stream viewer (streaming-quality stage 7): the header
// extension that carries the audio's capture time on the video clock, the
// decision rule (libwebrtc StreamSynchronization, audio master), and the
// rule against the real receive timeline on a simulated clock.

#include <gtest/gtest.h>

#include "audio/stream_audio_link.hpp"
#include "transport/av_sync.hpp"
#include "transport/peer_connection_impl.hpp"
#include "transport/rtp_av_sync_extension.hpp"
#include "transport/video_receive_timeline.hpp"

#include <chrono>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

using mello::transport::add_av_sync_extension;
using mello::transport::AudioSyncSample;
using mello::transport::AvSync;
using mello::transport::AvSyncInput;
using mello::transport::read_av_sync_extension;
using mello::transport::TimelineInput;
using mello::transport::VideoReceiveTimeline;

namespace {

std::vector<std::byte> rtp(size_t csrcs = 0, size_t payload = 20) {
    std::vector<std::byte> p(12 + csrcs * 4 + payload, std::byte{0x55});
    p[0] = std::byte{static_cast<uint8_t>(0x80 | csrcs)};
    p[1] = std::byte{111};
    return p;
}

std::optional<uint32_t> read(const std::vector<std::byte>& p) {
    return read_av_sync_extension(reinterpret_cast<const uint8_t*>(p.data()), p.size());
}

} // namespace

// ---- The header extension ----

TEST(AvSyncExtension, APacketWithoutAnExtensionGetsAOneByteBlock) {
    auto p = rtp();
    const auto payload_before = std::vector<std::byte>(p.begin() + 12, p.end());
    ASSERT_TRUE(add_av_sync_extension(p, 0xDEADBEEF));
    EXPECT_EQ(p.size(), 12u + 12u + payload_before.size());
    EXPECT_EQ(std::to_integer<int>(p[0]) & 0x10, 0x10) << "X bit";
    EXPECT_EQ(read(p), 0xDEADBEEFu);
    // The payload follows the block unchanged.
    EXPECT_TRUE(std::equal(payload_before.begin(), payload_before.end(), p.begin() + 24));
}

TEST(AvSyncExtension, AnExistingOneByteBlockGetsTheElementAppended) {
    auto p = rtp();
    // A block with one 2-byte element of ID 1 and padding.
    const std::byte block[8] = {std::byte{0xBE}, std::byte{0xDE}, std::byte{0}, std::byte{1},
                                std::byte{0x11}, std::byte{0xAA}, std::byte{0xBB}, std::byte{0}};
    p.insert(p.begin() + 12, block, block + 8);
    p[0] = p[0] | std::byte{0x10};
    ASSERT_TRUE(add_av_sync_extension(p, 1234567));
    EXPECT_EQ(std::to_integer<int>(p[15]), 3) << "the block grew by two words";
    EXPECT_EQ(read(p), 1234567u);
    EXPECT_EQ(std::to_integer<int>(p[17]), 0xAA) << "the old element stays";
}

TEST(AvSyncExtension, CsrcsAreSkipped) {
    auto p = rtp(2);
    ASSERT_TRUE(add_av_sync_extension(p, 42));
    EXPECT_EQ(read(p), 42u);
}

TEST(AvSyncExtension, ATwoByteBlockIsReadAndNotExtended) {
    auto p = rtp();
    const std::byte block[12] = {std::byte{0x10}, std::byte{0x00}, std::byte{0}, std::byte{2},
                                 std::byte{5},    std::byte{4},    std::byte{0}, std::byte{0},
                                 std::byte{1},    std::byte{2},    std::byte{0}, std::byte{0}};
    p.insert(p.begin() + 12, block, block + 12);
    p[0] = p[0] | std::byte{0x10};
    EXPECT_EQ(read(p), 0x00000102u);
    const auto before = p;
    EXPECT_FALSE(add_av_sync_extension(p, 7));
    EXPECT_EQ(p, before);
}

TEST(AvSyncExtension, NoExtensionReadsNothing) {
    const auto p = rtp();
    EXPECT_FALSE(read(p).has_value());
    const uint8_t junk[5] = {0x90, 111, 0, 0, 0};
    EXPECT_FALSE(read_av_sync_extension(junk, sizeof(junk)).has_value());
}

// ---- The decision rule ----

TEST(AvSync, AnOffsetInsideTheDeadBandChangesNothing) {
    AvSync sync;
    for (int i = 0; i < 20; ++i) {
        const auto d = sync.update(15.0, 40.0);
        EXPECT_FALSE(d.changed);
    }
    EXPECT_EQ(sync.video_min_delay_ms(), 0.0);
    EXPECT_EQ(sync.audio_min_delay_ms(), 0);
}

// The audio is 60 ms late: the video minimum rises from the timeline's own
// delay in steps of at most 10 ms until the rest is inside the dead band,
// and it never passes the timeline's maximum.
TEST(AvSync, LateAudioRaisesTheVideoMinimumInSmallSteps) {
    AvSync sync;
    const double video_delay = 40.0;
    double offset = 60.0;
    double last_min = 0.0;
    int steps = 0;
    for (int i = 0; i < 40; ++i) {
        const auto d = sync.update(offset, std::max(video_delay, sync.video_min_delay_ms()));
        if (d.changed) {
            ++steps;
            const double base = last_min > 0.0 ? last_min : video_delay;
            EXPECT_LE(d.video_min_delay_ms - base, 10.0 + 1e-9) << "a step over 10 ms";
            // The video now waits that much longer: the offset falls by it.
            offset -= d.video_min_delay_ms - base;
            last_min = d.video_min_delay_ms;
        }
    }
    EXPECT_GE(steps, 3);
    EXPECT_LE(std::abs(offset), 30.0);
    EXPECT_LE(sync.video_min_delay_ms(), 150.0);
    EXPECT_EQ(sync.audio_min_delay_ms(), 0);

    AvSync far;
    for (int i = 0; i < 200; ++i) far.update(400.0, 140.0);
    EXPECT_LE(far.video_min_delay_ms(), 150.0) << "the minimum passed the timeline's maximum";
}

// The video is late: a binding video minimum comes down first, then the
// audio gets delayed (NetEQ's minimum delay), because the video cannot be
// earlier than its own target. The model: the timeline's own delay is
// 40 ms, the video shows at max(40, minimum), the audio is heard after its
// end-to-end delay plus its minimum.
TEST(AvSync, LateVideoLowersTheVideoMinimumThenDelaysTheAudio) {
    AvSync sync;
    double audio_e2e = 100.0;
    auto offset = [&] {
        return audio_e2e + sync.audio_min_delay_ms() - std::max(40.0, sync.video_min_delay_ms());
    };
    for (int i = 0; i < 40; ++i) sync.update(offset(), std::max(40.0, sync.video_min_delay_ms()));
    const double raised = sync.video_min_delay_ms();
    ASSERT_GT(raised, 60.0) << "the late audio raised the video minimum first";
    EXPECT_EQ(sync.audio_min_delay_ms(), 0);

    // Now the audio is faster than the video can be.
    audio_e2e = 0.0;
    double lowest_video_min = raised;
    bool audio_while_video_binding = false;
    for (int i = 0; i < 80; ++i) {
        const double before = sync.video_min_delay_ms();
        sync.update(offset(), std::max(40.0, sync.video_min_delay_ms()));
        if (sync.audio_min_delay_ms() > 0 && before > 45.0) audio_while_video_binding = true;
        lowest_video_min = std::min(lowest_video_min, sync.video_min_delay_ms());
    }
    EXPECT_LT(lowest_video_min, 41.0) << "the binding video minimum did not come down";
    EXPECT_FALSE(audio_while_video_binding) << "audio delayed while the video minimum could fall";
    EXPECT_GT(sync.audio_min_delay_ms(), 0) << "the late video got no audio delay";
    EXPECT_LE(sync.audio_min_delay_ms(), 150);
    EXPECT_LE(std::abs(offset()), 30.0);
}

TEST(AvSync, AnOffsetOverOneSecondIsIgnored) {
    AvSync sync;
    for (int i = 0; i < 10; ++i) EXPECT_FALSE(sync.update(5000.0, 40.0).changed);
    EXPECT_FALSE(sync.filtered_offset_ms().has_value());
}

// ---- The rule against the receive timeline ----

namespace {

struct SimClock {
    int64_t now = 50'000'000;
    VideoReceiveTimeline::ClockFn fn() {
        return [this] { return now; };
    }
};

TimelineInput key_or_delta(int64_t id, uint32_t rtp, int64_t arrival_us) {
    TimelineInput in;
    in.id = id;
    in.reference = id == 0 ? -1 : id - 1;
    in.rtp_timestamp = rtp;
    in.arrival_us = arrival_us;
    in.keyframe = id == 0;
    in.annex_b.assign(4000, 0x42);
    return in;
}

} // namespace

// A 60 fps stream whose audio is heard 120 ms after its capture, while the
// video of the same instant is shown after the timeline's own delay (about
// 20 ms here): the A/V sync raises the timeline's minimum delay until the
// offset is inside the dead band, and the render times follow. Audio that
// is faster than the video afterwards takes the minimum back down. Before
// stage 7 nothing set the timeline's minimum delay.
TEST(AvSyncInput, TheTimelineFollowsTheAudio) {
    SimClock clock;
    VideoReceiveTimeline::Config config;
    auto timeline_ptr = std::make_shared<VideoReceiveTimeline>(config, clock.fn());
    VideoReceiveTimeline& timeline = *timeline_ptr;

    // The audio: heard `audio_e2e_ms` after its capture, plus the delay the
    // sync gives it.
    double audio_e2e_ms = 120.0;
    int audio_extra_ms = 0;
    const int64_t start_us = clock.now;
    AvSyncInput input(
        [&]() -> std::optional<AudioSyncSample> {
            // The video timestamp of the instant captured that long ago.
            const double age_ms = audio_e2e_ms + static_cast<double>(audio_extra_ms);
            const double t_ms = static_cast<double>(clock.now - start_us) / 1000.0 - age_ms;
            AudioSyncSample s;
            s.video_rtp_timestamp = static_cast<uint32_t>(static_cast<int64_t>(t_ms * 90.0));
            s.heard_at_us = clock.now;
            return s;
        },
        [&](int ms) { audio_extra_ms = ms; });

    int64_t id = 0;
    auto run_ms = [&](int64_t ms) {
        const int64_t until = clock.now + ms * 1000;
        while (clock.now < until) {
            // Frame id arrives at start + id * 16.667 ms with the RTP
            // timestamp of its capture.
            const int64_t due = start_us + id * 16'667;
            if (clock.now >= due) {
                timeline.insert(key_or_delta(id, static_cast<uint32_t>(id * 1500), clock.now));
                ++id;
                // Decode side: release what is due (no decode cost).
                for (;;) {
                    auto d = timeline.poll();
                    if (d.kind != mello::transport::TimelineDecision::Kind::Release) break;
                    timeline.on_decoded(1000);
                }
            }
            input.tick(timeline_ptr, clock.now);
            clock.now += 1000;
        }
    };

    run_ms(200);
    const double before = timeline.stats().current_delay_ms;
    ASSERT_LT(before, 40.0) << "the timeline's own delay";
    run_ms(8800);
    auto stats = input.stats();
    EXPECT_TRUE(stats.active);
    EXPECT_GT(stats.changes, 3u);
    EXPECT_LE(std::abs(stats.filtered_offset_ms), 30.0) << "not in sync after 9 s";
    EXPECT_GT(stats.video_min_delay_ms, before + 50.0) << "the video minimum did not follow";
    EXPECT_EQ(stats.audio_min_delay_ms, 0);
    EXPECT_GE(timeline.stats().current_delay_ms, stats.video_min_delay_ms - 1.0);

    // Now the audio is faster than the video: the video minimum comes back
    // down.
    audio_e2e_ms = 10.0;
    const double raised = stats.video_min_delay_ms;
    run_ms(8000);
    stats = input.stats();
    EXPECT_LT(stats.video_min_delay_ms, raised - 50.0);
    EXPECT_LE(std::abs(stats.filtered_offset_ms), 30.0);
}

// The audio stops (a pause): after 3 s the delays go back to none.
TEST(AvSyncInput, NoAudioResetsTheDelays) {
    SimClock clock;
    auto timeline_ptr =
        std::make_shared<VideoReceiveTimeline>(VideoReceiveTimeline::Config{}, clock.fn());
    VideoReceiveTimeline& timeline = *timeline_ptr;
    bool audio = true;
    const int64_t start_us = clock.now;
    AvSyncInput input(
        [&]() -> std::optional<AudioSyncSample> {
            if (!audio) return std::nullopt;
            const double t_ms = static_cast<double>(clock.now - start_us) / 1000.0 - 200.0;
            AudioSyncSample s;
            s.video_rtp_timestamp = static_cast<uint32_t>(static_cast<int64_t>(t_ms * 90.0));
            s.heard_at_us = clock.now;
            return s;
        },
        nullptr);
    for (int64_t id = 0; id < 300; ++id) {
        clock.now = start_us + id * 16'667;
        timeline.insert(key_or_delta(id, static_cast<uint32_t>(id * 1500), clock.now));
        while (timeline.poll().kind == mello::transport::TimelineDecision::Kind::Release) {
            timeline.on_decoded(1000);
        }
        input.tick(timeline_ptr, clock.now);
    }
    ASSERT_GT(input.stats().video_min_delay_ms, 0.0);
    audio = false;
    for (int64_t id = 300; id < 600; ++id) {
        clock.now = start_us + id * 16'667;
        input.tick(timeline_ptr, clock.now);
    }
    EXPECT_EQ(input.stats().video_min_delay_ms, 0.0);
    EXPECT_FALSE(input.stats().active);
}

// ---- On the wire: host peer to viewer peer ----

namespace {

using namespace std::chrono_literals;
using mello::transport::PeerConnectionImpl;

struct Candidates {
    std::mutex mutex;
    std::vector<std::pair<std::string, std::string>> items;  // candidate, mid
};

void on_candidate(void* user_data, const MelloIceCandidate* c) {
    if (!user_data || !c || !c->candidate) return;
    auto* q = static_cast<Candidates*>(user_data);
    std::lock_guard<std::mutex> lock(q->mutex);
    q->items.emplace_back(c->candidate, c->sdp_mid ? c->sdp_mid : "");
}

void drain(Candidates& from, PeerConnectionImpl& to) {
    std::vector<std::pair<std::string, std::string>> pending;
    {
        std::lock_guard<std::mutex> lock(from.mutex);
        pending.swap(from.items);
    }
    for (const auto& [candidate, mid] : pending) to.add_ice_candidate(candidate, mid, 0);
}

template <typename Predicate>
bool wait_for(Predicate predicate, std::chrono::milliseconds timeout) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (std::chrono::steady_clock::now() < deadline) {
        if (predicate()) return true;
        std::this_thread::sleep_for(5ms);
    }
    return predicate();
}

struct ReceivedAudio {
    std::mutex mutex;
    std::vector<std::vector<uint8_t>> packets;
};

void on_audio(void* user_data, const char*, const uint8_t* data, int size) {
    auto* r = static_cast<ReceivedAudio*>(user_data);
    std::lock_guard<std::mutex> lock(r->mutex);
    r->packets.emplace_back(data, data + size);
}

} // namespace

// A stream host sends a video frame captured at T0 and a game-audio frame
// captured at T0 + 40 ms; the viewer's audio packet carries the video RTP
// timestamp of T0 + 40 ms: the frame's timestamp plus 3600 ticks. Before
// stage 7 the audio carried no capture time at all.
TEST(AvSyncWire, TheGameAudioCarriesItsCaptureTimeOnTheVideoClock) {
    auto viewer = std::make_shared<PeerConnectionImpl>("viewer", MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER);
    auto host = std::make_shared<PeerConnectionImpl>("host", MELLO_PEER_MEDIA_ROLE_STREAM_HOST);
    // What mello_peer_create_for_role(ctx, ..., STREAM_HOST) does.
    auto link = std::make_shared<mello::audio::StreamAudioLink>();
    host->set_stream_audio_link(link);
    Candidates from_viewer;
    Candidates from_host;
    viewer->set_ice_servers({});
    host->set_ice_servers({});
    viewer->set_ice_callback(on_candidate, &from_viewer);
    host->set_ice_callback(on_candidate, &from_host);
    ReceivedAudio received;
    viewer->set_audio_rtp_callback(on_audio, &received);

    // The client's order: the viewer offers, the host answers.
    const char* offer = viewer->create_offer();
    ASSERT_NE(offer, nullptr);
    const std::string offer_sdp(offer);
    EXPECT_NE(offer_sdp.find(mello::transport::kAvSyncExtensionUri), std::string::npos)
        << "the audio m-line declares the extension";
    const char* answer = host->create_answer(offer_sdp.c_str());
    ASSERT_NE(answer, nullptr);
    const std::string answer_sdp(answer);
    ASSERT_TRUE(viewer->set_remote_description(answer_sdp.c_str(), false));
    ASSERT_TRUE(wait_for(
        [&] {
            drain(from_viewer, *host);
            drain(from_host, *viewer);
            return host->is_connected() && viewer->is_connected() && host->video_is_open() &&
                   viewer->video_is_open();
        },
        10s));

    // One IDR captured at T0 sets the video clock.
    constexpr uint64_t kT0 = 5'000'000;
    const std::vector<uint8_t> idr = {0, 0, 0, 1, 0x67, 0x4d, 0x00, 0x2a, 0, 0, 0, 1, 0x68, 0xce,
                                      0x38, 0x80, 0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00};
    ASSERT_TRUE(wait_for(
        [&] {
            return host->video_send_access_unit(idr.data(), idr.size(), kT0) ==
                   mello::transport::SendAccessUnitResult::Accepted;
        },
        5s));
    MelloRtpVideoAccessUnitInfo info{};
    std::vector<uint8_t> buffer(64 * 1024);
    ASSERT_TRUE(wait_for(
        [&] {
            return viewer->video_recv_access_unit(buffer.data(), buffer.size(), &info) > 0;
        },
        5s));

    // The host pipeline recorded the capture time of the frame with media
    // time 9600; the peer sends it.
    constexpr uint32_t kMedia = 9600;
    link->record_capture(kMedia, static_cast<int64_t>(kT0 + 40'000));
    const uint8_t opus[3] = {0xFC, 0xFF, 0xFE};
    ASSERT_TRUE(wait_for([&] { return host->send_audio_frame(opus, 3, kMedia); }, 5s));
    ASSERT_TRUE(wait_for(
        [&] {
            std::lock_guard<std::mutex> lock(received.mutex);
            return !received.packets.empty();
        },
        5s));
    std::lock_guard<std::mutex> lock(received.mutex);
    const auto& pkt = received.packets.back();
    const auto video_ts = read_av_sync_extension(pkt.data(), pkt.size());
    ASSERT_TRUE(video_ts.has_value()) << "no A/V sync extension on the game audio";
    EXPECT_EQ(*video_ts - info.rtp_timestamp, 3600u);
    EXPECT_GE(host->av_sync_extensions(), 1u);
}

// The start buffer is the time from when the audio could be heard to when
// the video of its capture instant is shown, and it becomes the audio's
// minimum delay. Before the first video frame there is none.
TEST(AvSyncInput, TheStartBufferIsTheVideoDelayOverTheAudio) {
    SimClock clock;
    auto timeline_ptr =
        std::make_shared<VideoReceiveTimeline>(VideoReceiveTimeline::Config{}, clock.fn());
    AvSyncInput input([]() -> std::optional<AudioSyncSample> { return std::nullopt; }, nullptr);
    EXPECT_FALSE(input.start_buffer_ms(0, clock.now).has_value()) << "no timeline yet";
    input.tick(timeline_ptr, clock.now);
    EXPECT_FALSE(input.start_buffer_ms(0, clock.now).has_value()) << "no frame yet";
    const int64_t start_us = clock.now;
    for (int64_t id = 0; id < 60; ++id) {
        clock.now = start_us + id * 16'667;
        timeline_ptr->insert(key_or_delta(id, static_cast<uint32_t>(id * 1500), clock.now));
        while (timeline_ptr->poll().kind == mello::transport::TimelineDecision::Kind::Release) {
            timeline_ptr->on_decoded(1000);
        }
    }
    // Frame 59 is shown at its arrival plus the timeline's delay.
    const auto shown = timeline_ptr->render_time_us(59 * 1500);
    ASSERT_TRUE(shown.has_value());
    const int64_t heard = clock.now + 5'000;
    const auto buffer = input.start_buffer_ms(59 * 1500, heard);
    ASSERT_TRUE(buffer.has_value());
    EXPECT_NEAR(*buffer, static_cast<double>(*shown - heard) / 1000.0, 1.0);
    EXPECT_EQ(input.stats().audio_min_delay_ms, *buffer);
}
