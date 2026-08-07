#include <gtest/gtest.h>

#include "mello.h"

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

namespace {

using namespace std::chrono_literals;

struct IceRelay {
    std::mutex mutex;
    std::vector<std::tuple<std::string, std::string, int>> candidates;
};

IceRelay g_relay_a_to_b;
IceRelay g_relay_b_to_a;

void ice_cb_a(void*, const MelloIceCandidate* candidate) {
    if (candidate == nullptr || candidate->candidate == nullptr) {
        return;
    }
    std::lock_guard<std::mutex> lock(g_relay_a_to_b.mutex);
    g_relay_a_to_b.candidates.emplace_back(
        candidate->candidate,
        candidate->sdp_mid ? candidate->sdp_mid : "",
        candidate->sdp_mline_index
    );
}

void ice_cb_b(void*, const MelloIceCandidate* candidate) {
    if (candidate == nullptr || candidate->candidate == nullptr) {
        return;
    }
    std::lock_guard<std::mutex> lock(g_relay_b_to_a.mutex);
    g_relay_b_to_a.candidates.emplace_back(
        candidate->candidate,
        candidate->sdp_mid ? candidate->sdp_mid : "",
        candidate->sdp_mline_index
    );
}

void drain_candidates(IceRelay& relay, MelloPeerConnection* target_peer) {
    std::vector<std::tuple<std::string, std::string, int>> pending;
    {
        std::lock_guard<std::mutex> lock(relay.mutex);
        pending.swap(relay.candidates);
    }
    for (const auto& entry : pending) {
        MelloIceCandidate candidate{};
        candidate.candidate = std::get<0>(entry).c_str();
        candidate.sdp_mid = std::get<1>(entry).c_str();
        candidate.sdp_mline_index = std::get<2>(entry);
        mello_peer_add_ice_candidate(target_peer, &candidate);
    }
}

struct NegotiatedStreamPair {
    MelloPeerConnection* host = nullptr;
    MelloPeerConnection* viewer = nullptr;

    NegotiatedStreamPair() = default;
    NegotiatedStreamPair(NegotiatedStreamPair&& other) noexcept
        : host(other.host),
          viewer(other.viewer) {
        other.host = nullptr;
        other.viewer = nullptr;
    }
    NegotiatedStreamPair& operator=(NegotiatedStreamPair&& other) noexcept {
        if (this == &other) {
            return *this;
        }
        if (viewer) {
            mello_peer_destroy(viewer);
        }
        if (host) {
            mello_peer_destroy(host);
        }
        host = other.host;
        viewer = other.viewer;
        other.host = nullptr;
        other.viewer = nullptr;
        return *this;
    }
    NegotiatedStreamPair(const NegotiatedStreamPair&) = delete;
    NegotiatedStreamPair& operator=(const NegotiatedStreamPair&) = delete;

    ~NegotiatedStreamPair() {
        if (viewer) {
            mello_peer_destroy(viewer);
        }
        if (host) {
            mello_peer_destroy(host);
        }
    }
};

NegotiatedStreamPair negotiate_host_offer(
    const char* host_id = "host",
    const char* viewer_id = "viewer"
) {
    NegotiatedStreamPair pair;
    pair.host = mello_peer_create_for_role(
        nullptr,
        host_id,
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    if (pair.host == nullptr) {
        return pair;
    }
    mello_peer_set_ice_servers(pair.host, nullptr, 0);
    mello_peer_set_ice_callback(pair.host, ice_cb_a, nullptr);

    const char* offer_ptr = mello_peer_create_offer(pair.host);
    if (offer_ptr == nullptr) {
        return pair;
    }
    const std::string offer(offer_ptr);

    pair.viewer = mello_peer_create_for_role(
        nullptr,
        viewer_id,
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    if (pair.viewer == nullptr) {
        return pair;
    }
    mello_peer_set_ice_servers(pair.viewer, nullptr, 0);
    mello_peer_set_ice_callback(pair.viewer, ice_cb_b, nullptr);

    const char* answer_ptr = mello_peer_create_answer(pair.viewer, offer.c_str());
    if (answer_ptr == nullptr) {
        return pair;
    }
    const std::string answer(answer_ptr);
    if (mello_peer_set_remote_description(pair.host, answer.c_str(), false) != MELLO_OK) {
        mello_peer_destroy(pair.viewer);
        pair.viewer = nullptr;
        return pair;
    }
    return pair;
}

NegotiatedStreamPair negotiate_viewer_offer(
    const char* viewer_id = "viewer",
    const char* host_id = "host"
) {
    NegotiatedStreamPair pair;
    pair.viewer = mello_peer_create_for_role(
        nullptr,
        viewer_id,
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    if (pair.viewer == nullptr) {
        return pair;
    }
    mello_peer_set_ice_servers(pair.viewer, nullptr, 0);
    mello_peer_set_ice_callback(pair.viewer, ice_cb_b, nullptr);

    const char* offer_ptr = mello_peer_create_offer(pair.viewer);
    if (offer_ptr == nullptr) {
        return pair;
    }
    const std::string offer(offer_ptr);

    pair.host = mello_peer_create_for_role(
        nullptr,
        host_id,
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    if (pair.host == nullptr) {
        return pair;
    }
    mello_peer_set_ice_servers(pair.host, nullptr, 0);
    mello_peer_set_ice_callback(pair.host, ice_cb_a, nullptr);

    const char* answer_ptr = mello_peer_create_answer(pair.host, offer.c_str());
    if (answer_ptr == nullptr) {
        return pair;
    }
    const std::string answer(answer_ptr);
    if (mello_peer_set_remote_description(pair.viewer, answer.c_str(), false) != MELLO_OK) {
        mello_peer_destroy(pair.host);
        pair.host = nullptr;
        return pair;
    }
    return pair;
}

template <typename Predicate>
bool wait_until(Predicate predicate, std::chrono::milliseconds timeout) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (std::chrono::steady_clock::now() < deadline) {
        if (predicate()) {
            return true;
        }
        std::this_thread::sleep_for(5ms);
    }
    return predicate();
}

bool exchange_ice(MelloPeerConnection* peer_a, MelloPeerConnection* peer_b) {
    return wait_until(
        [&]() {
            drain_candidates(g_relay_a_to_b, peer_b);
            drain_candidates(g_relay_b_to_a, peer_a);
            return mello_peer_is_connected(peer_a) && mello_peer_is_connected(peer_b);
        },
        5s
    );
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
    return annex_b({{0x67, 0x4d, 0x00, 0x2a},
                    {0x68, 0xce, 0x38, 0x80},
                    {0x65, 0x88, 0x84, 0x00}});
}

std::vector<uint8_t> make_delta_access_unit(uint8_t suffix) {
    return annex_b({{0x61, suffix, 0x10, 0x20}});
}

/// A delta access unit of a chosen payload size, for tests that need to fill
/// the sender queue by volume rather than by frame count.
std::vector<uint8_t> make_large_delta_access_unit(size_t nal_payload_bytes) {
    std::vector<uint8_t> nal(nal_payload_bytes, 0x55);
    nal[0] = 0x61;
    return annex_b({nal});
}

void expect_stream_sdp(const char* sdp, const char* expected_direction) {
    ASSERT_NE(sdp, nullptr);
    const std::string text(sdp);
    EXPECT_NE(text.find("m=video"), std::string::npos);
    EXPECT_NE(text.find("H264/90000"), std::string::npos);
    EXPECT_NE(text.find("profile-level-id=4d002a"), std::string::npos);
    EXPECT_NE(text.find("packetization-mode=1"), std::string::npos);
    EXPECT_NE(text.find("level-asymmetry-allowed=1"), std::string::npos);
    EXPECT_NE(text.find(expected_direction), std::string::npos);
    EXPECT_EQ(text.find("m=audio"), std::string::npos);
    EXPECT_EQ(text.find("opus/48000"), std::string::npos);
}

size_t count_media_sections(const std::string& sdp, const std::string& type) {
    const std::string marker = "m=" + type;
    size_t count = 0;
    size_t offset = 0;
    while ((offset = sdp.find(marker, offset)) != std::string::npos) {
        if (offset == 0 || sdp[offset - 1] == '\n') {
            ++count;
        }
        offset += marker.size();
    }
    return count;
}

int media_index_for_mid(const std::string& sdp, const std::string& mid) {
    const std::string mid_line = "a=mid:" + mid;
    int index = -1;
    size_t section = sdp.find("m=");
    while (section != std::string::npos) {
        ++index;
        const size_t next = sdp.find("\nm=", section);
        const size_t end =
            next == std::string::npos ? sdp.size() : next + 1;
        if (sdp.find(mid_line, section) < end) {
            return index;
        }
        section = next == std::string::npos ? std::string::npos : next + 1;
    }
    return -1;
}

std::string duplicate_video_section(const std::string& sdp) {
    const size_t begin = sdp.find("m=video");
    if (begin == std::string::npos) {
        return sdp;
    }
    const size_t next = sdp.find("\nm=", begin);
    const size_t end = next == std::string::npos ? sdp.size() : next + 1;
    std::string duplicate = sdp.substr(begin, end - begin);
    const size_t mid = duplicate.find("a=mid:video");
    if (mid != std::string::npos) {
        duplicate.replace(mid, std::strlen("a=mid:video"), "a=mid:video-copy");
    }
    return sdp + duplicate;
}

std::string remove_video_section(const std::string& sdp) {
    const size_t begin = sdp.find("m=video");
    if (begin == std::string::npos) {
        return sdp;
    }
    const size_t next = sdp.find("\nm=", begin);
    const size_t end = next == std::string::npos ? sdp.size() : next + 1;
    std::string result = sdp;
    result.erase(begin, end - begin);
    return result;
}

bool wait_stream_video_ready(
    MelloPeerConnection* host,
    MelloPeerConnection* viewer
) {
    return wait_until(
        [&]() {
            return mello_peer_video_is_open(host) != 0
                && mello_peer_video_is_open(viewer) != 0;
        },
        5s
    );
}

class PeerRtpRolesTest : public ::testing::Test {
protected:
    void SetUp() override {
        {
            std::lock_guard<std::mutex> lock(g_relay_a_to_b.mutex);
            g_relay_a_to_b.candidates.clear();
        }
        {
            std::lock_guard<std::mutex> lock(g_relay_b_to_a.mutex);
            g_relay_b_to_a.candidates.clear();
        }
    }
};

TEST_F(PeerRtpRolesTest, VoiceWrapperCreatesVoiceRolePeer) {
    auto* peer = mello_peer_create(nullptr, "voice-peer");
    ASSERT_NE(peer, nullptr);
    mello_peer_destroy(peer);
}

TEST_F(PeerRtpRolesTest, StreamHostOfferContainsSendonlyH264) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);

    const char* offer = mello_peer_create_offer(host);
    expect_stream_sdp(offer, "sendonly");
    EXPECT_EQ(mello_peer_is_unreliable_open(host), false);

    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, StreamViewerOfferContainsRecvonlyH264) {
    auto* viewer = mello_peer_create_for_role(
        nullptr,
        "viewer",
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    ASSERT_NE(viewer, nullptr);
    mello_peer_set_ice_servers(viewer, nullptr, 0);

    const char* offer = mello_peer_create_offer(viewer);
    expect_stream_sdp(offer, "recvonly");

    mello_peer_destroy(viewer);
}

TEST_F(PeerRtpRolesTest, HostOfferViewerAnswerNegotiates) {
    auto pair = negotiate_host_offer();
    ASSERT_NE(pair.host, nullptr);
    ASSERT_NE(pair.viewer, nullptr);
}

TEST_F(PeerRtpRolesTest, HostOfferViewerAnswerConnects) {
    auto pair = negotiate_host_offer();
    ASSERT_NE(pair.host, nullptr);
    ASSERT_NE(pair.viewer, nullptr);
    EXPECT_EQ(mello_peer_video_is_open(pair.host), 0u);
    EXPECT_EQ(mello_peer_video_is_open(pair.viewer), 0u);
    ASSERT_TRUE(exchange_ice(pair.host, pair.viewer));
    ASSERT_TRUE(wait_stream_video_ready(pair.host, pair.viewer));
}

TEST_F(PeerRtpRolesTest, ViewerOfferHostAnswerNegotiates) {
    auto pair = negotiate_viewer_offer();
    ASSERT_NE(pair.host, nullptr);
    ASSERT_NE(pair.viewer, nullptr);
    ASSERT_TRUE(exchange_ice(pair.host, pair.viewer));
    ASSERT_TRUE(wait_stream_video_ready(pair.host, pair.viewer));
}

TEST_F(PeerRtpRolesTest, VoicePeerRejectsStreamHostOffer) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);
    const char* offer = mello_peer_create_offer(host);
    ASSERT_NE(offer, nullptr);

    auto* voice = mello_peer_create(nullptr, "voice");
    ASSERT_NE(voice, nullptr);
    mello_peer_set_ice_servers(voice, nullptr, 0);
    EXPECT_EQ(mello_peer_create_answer(voice, offer), nullptr);

    mello_peer_destroy(voice);
    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, StreamViewerRejectsVoiceOffer) {
    auto* voice = mello_peer_create(nullptr, "voice");
    ASSERT_NE(voice, nullptr);
    mello_peer_set_ice_servers(voice, nullptr, 0);
    const char* offer = mello_peer_create_offer(voice);
    ASSERT_NE(offer, nullptr);

    auto* viewer = mello_peer_create_for_role(
        nullptr,
        "viewer",
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    ASSERT_NE(viewer, nullptr);
    mello_peer_set_ice_servers(viewer, nullptr, 0);
    EXPECT_EQ(mello_peer_create_answer(viewer, offer), nullptr);

    mello_peer_destroy(viewer);
    mello_peer_destroy(voice);
}

TEST_F(PeerRtpRolesTest, StreamViewerRejectsOfferWithoutVideo) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);
    const char* offer_ptr = mello_peer_create_offer(host);
    ASSERT_NE(offer_ptr, nullptr);
    const std::string offer_without_video =
        remove_video_section(offer_ptr);

    auto* viewer = mello_peer_create_for_role(
        nullptr,
        "viewer",
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    ASSERT_NE(viewer, nullptr);
    mello_peer_set_ice_servers(viewer, nullptr, 0);
    EXPECT_EQ(
        mello_peer_create_answer(viewer, offer_without_video.c_str()),
        nullptr
    );

    mello_peer_destroy(viewer);
    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, StreamViewerRejectsMultipleVideoSections) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);
    const char* offer_ptr = mello_peer_create_offer(host);
    ASSERT_NE(offer_ptr, nullptr);
    const std::string duplicate_offer =
        duplicate_video_section(offer_ptr);
    ASSERT_EQ(count_media_sections(duplicate_offer, "video"), 2u);

    auto* viewer = mello_peer_create_for_role(
        nullptr,
        "viewer",
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    ASSERT_NE(viewer, nullptr);
    mello_peer_set_ice_servers(viewer, nullptr, 0);
    EXPECT_EQ(
        mello_peer_create_answer(viewer, duplicate_offer.c_str()),
        nullptr
    );

    mello_peer_destroy(viewer);
    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, ViewerAnswererHandlesRepeatedHostOffers) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);
    mello_peer_set_ice_callback(host, ice_cb_a, nullptr);
    const char* offer_ptr = mello_peer_create_offer(host);
    ASSERT_NE(offer_ptr, nullptr);
    const std::string offer(offer_ptr);

    auto* viewer = mello_peer_create_for_role(
        nullptr,
        "viewer",
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    ASSERT_NE(viewer, nullptr);
    mello_peer_set_ice_servers(viewer, nullptr, 0);
    mello_peer_set_ice_callback(viewer, ice_cb_b, nullptr);
    const char* initial_answer_ptr =
        mello_peer_create_answer(viewer, offer.c_str());
    ASSERT_NE(initial_answer_ptr, nullptr);
    const std::string initial_answer(initial_answer_ptr);
    ASSERT_EQ(
        mello_peer_set_remote_description(
            host,
            initial_answer.c_str(),
            false
        ),
        MELLO_OK
    );
    ASSERT_TRUE(exchange_ice(host, viewer));
    ASSERT_TRUE(wait_stream_video_ready(host, viewer));
    for (int iteration = 0; iteration < 2; ++iteration) {
        const char* answer_ptr =
            mello_peer_handle_remote_offer(viewer, offer.c_str());
        ASSERT_NE(answer_ptr, nullptr);
        const std::string answer(answer_ptr);
        EXPECT_EQ(count_media_sections(answer, "video"), 1u);
        EXPECT_TRUE(wait_stream_video_ready(host, viewer));
    }
    mello_peer_destroy(viewer);
    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, HostAnswererHandlesRepeatedViewerOffers) {
    auto* viewer = mello_peer_create_for_role(
        nullptr,
        "viewer",
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    ASSERT_NE(viewer, nullptr);
    mello_peer_set_ice_servers(viewer, nullptr, 0);
    mello_peer_set_ice_callback(viewer, ice_cb_b, nullptr);
    const char* offer_ptr = mello_peer_create_offer(viewer);
    ASSERT_NE(offer_ptr, nullptr);
    const std::string offer(offer_ptr);

    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);
    mello_peer_set_ice_callback(host, ice_cb_a, nullptr);
    const char* initial_answer_ptr =
        mello_peer_create_answer(host, offer.c_str());
    ASSERT_NE(initial_answer_ptr, nullptr);
    const std::string initial_answer(initial_answer_ptr);
    ASSERT_EQ(
        mello_peer_set_remote_description(
            viewer,
            initial_answer.c_str(),
            false
        ),
        MELLO_OK
    );
    ASSERT_TRUE(exchange_ice(host, viewer));
    ASSERT_TRUE(wait_stream_video_ready(host, viewer));
    for (int iteration = 0; iteration < 2; ++iteration) {
        const char* answer_ptr =
            mello_peer_handle_remote_offer(host, offer.c_str());
        ASSERT_NE(answer_ptr, nullptr);
        const std::string answer(answer_ptr);
        EXPECT_EQ(count_media_sections(answer, "video"), 1u);
        EXPECT_TRUE(wait_stream_video_ready(host, viewer));
    }
    mello_peer_destroy(host);
    mello_peer_destroy(viewer);
}

TEST_F(PeerRtpRolesTest, InvalidRenegotiationLeavesViewerRecoverable) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);
    const char* offer_ptr = mello_peer_create_offer(host);
    ASSERT_NE(offer_ptr, nullptr);
    const std::string valid_offer(offer_ptr);
    const std::string invalid_offer =
        duplicate_video_section(valid_offer);

    auto* viewer = mello_peer_create_for_role(
        nullptr,
        "viewer",
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    ASSERT_NE(viewer, nullptr);
    mello_peer_set_ice_servers(viewer, nullptr, 0);
    ASSERT_NE(mello_peer_create_answer(viewer, valid_offer.c_str()), nullptr);
    EXPECT_EQ(
        mello_peer_handle_remote_offer(
            viewer,
            invalid_offer.c_str()
        ),
        nullptr
    );
    const char* answer_ptr =
        mello_peer_handle_remote_offer(viewer, valid_offer.c_str());
    ASSERT_NE(answer_ptr, nullptr);
    const std::string answer(answer_ptr);
    EXPECT_EQ(count_media_sections(answer, "video"), 1u);
    mello_peer_destroy(viewer);
    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, ConcurrentCreateOffersAreSerialized) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);

    std::atomic<int> successful_offers{0};
    auto create_offer = [&]() {
        if (mello_peer_create_offer(host) != nullptr) {
            successful_offers.fetch_add(1, std::memory_order_relaxed);
        }
    };
    std::thread first(create_offer);
    std::thread second(create_offer);
    first.join();
    second.join();

    EXPECT_EQ(successful_offers.load(std::memory_order_relaxed), 2);
    const char* final_offer = mello_peer_create_offer(host);
    expect_stream_sdp(final_offer, "sendonly");
    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, IceCandidateUsesNonnumericMidMediaIndex) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);
    mello_peer_set_ice_callback(host, ice_cb_a, nullptr);
    const char* offer_ptr = mello_peer_create_offer(host);
    ASSERT_NE(offer_ptr, nullptr);
    const std::string offer(offer_ptr);
    const int expected_index = media_index_for_mid(offer, "video");
    ASSERT_EQ(expected_index, 0);

    ASSERT_TRUE(wait_until(
        [&]() {
            std::lock_guard<std::mutex> lock(g_relay_a_to_b.mutex);
            for (const auto& candidate : g_relay_a_to_b.candidates) {
                if (std::get<1>(candidate) == "video") {
                    EXPECT_EQ(std::get<2>(candidate), expected_index);
                    return true;
                }
            }
            return false;
        },
        5s
    ));

    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, RecvAccessUnitUsesDedicatedErrorSentinel) {
    MelloRtpVideoAccessUnitInfo info{};
    info.size = 123;
    EXPECT_EQ(
        mello_peer_video_recv_access_unit(nullptr, nullptr, 0, &info),
        MELLO_PEER_VIDEO_RECV_ERROR
    );
    EXPECT_EQ(info.size, 0u);

    auto* viewer = mello_peer_create_for_role(
        nullptr,
        "viewer",
        MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER
    );
    ASSERT_NE(viewer, nullptr);
    info.size = 123;
    EXPECT_EQ(
        mello_peer_video_recv_access_unit(viewer, nullptr, -1, &info),
        MELLO_PEER_VIDEO_RECV_ERROR
    );
    EXPECT_EQ(info.size, 0u);
    mello_peer_destroy(viewer);
}

TEST_F(PeerRtpRolesTest, HostSendsAccessUnitViewerPolls) {
    auto pair = negotiate_host_offer();
    ASSERT_NE(pair.host, nullptr);
    ASSERT_NE(pair.viewer, nullptr);
    ASSERT_TRUE(exchange_ice(pair.host, pair.viewer));
    ASSERT_TRUE(wait_stream_video_ready(pair.host, pair.viewer));

    const auto idr = make_idr_access_unit();
    ASSERT_EQ(
        mello_peer_video_send_access_unit(
            pair.host,
            idr.data(),
            static_cast<int>(idr.size()),
            1'000
        ),
        MELLO_OK
    );

    std::vector<uint8_t> received;
    ASSERT_TRUE(wait_until(
        [&]() {
            MelloRtpVideoAccessUnitInfo info{};
            std::vector<uint8_t> buffer(256 * 1024);
            const int copied = mello_peer_video_recv_access_unit(
                pair.viewer,
                buffer.data(),
                static_cast<int>(buffer.size()),
                &info
            );
            if (copied > 0) {
                received.assign(buffer.begin(), buffer.begin() + copied);
                EXPECT_EQ(info.is_idr, 1u);
                EXPECT_GT(info.size, 0u);
                return true;
            }
            return false;
        },
        5s
    ));
    ASSERT_FALSE(received.empty());
}

TEST_F(PeerRtpRolesTest, RecvAccessUnitReportsRequiredCapacity) {
    auto pair = negotiate_host_offer();
    ASSERT_NE(pair.host, nullptr);
    ASSERT_NE(pair.viewer, nullptr);
    ASSERT_TRUE(exchange_ice(pair.host, pair.viewer));
    ASSERT_TRUE(wait_stream_video_ready(pair.host, pair.viewer));

    const auto idr = make_idr_access_unit();
    ASSERT_EQ(
        mello_peer_video_send_access_unit(
            pair.host,
            idr.data(),
            static_cast<int>(idr.size()),
            2'000
        ),
        MELLO_OK
    );

    ASSERT_TRUE(wait_until(
        [&]() {
            MelloRtpVideoAccessUnitInfo probe{};
            return mello_peer_video_recv_access_unit(
                       pair.viewer,
                       nullptr,
                       0,
                       &probe
                   ) < 0;
        },
        5s
    ));

    MelloRtpVideoAccessUnitInfo info{};
    const int required = mello_peer_video_recv_access_unit(
        pair.viewer,
        nullptr,
        8,
        &info
    );
    ASSERT_LT(required, 0);
    EXPECT_EQ(static_cast<size_t>(-required), info.size);

    std::vector<uint8_t> buffer(static_cast<size_t>(-required));
    const int copied = mello_peer_video_recv_access_unit(
        pair.viewer,
        buffer.data(),
        static_cast<int>(buffer.size()),
        &info
    );
    EXPECT_EQ(copied, -required);
    EXPECT_EQ(info.is_idr, 1u);
}

TEST_F(PeerRtpRolesTest, FeedbackQueueStartsEmpty) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);

    MelloPeerVideoFeedback feedback{};
    EXPECT_EQ(mello_peer_video_take_feedback(host, &feedback), 0u);
    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, BoundedBurstRequestsLocalIdrOnQueueOverflow) {
    auto pair = negotiate_host_offer();
    ASSERT_NE(pair.host, nullptr);
    ASSERT_NE(pair.viewer, nullptr);
    ASSERT_TRUE(exchange_ice(pair.host, pair.viewer));
    ASSERT_TRUE(wait_stream_video_ready(pair.host, pair.viewer));

    const auto idr = make_idr_access_unit();
    ASSERT_EQ(
        mello_peer_video_send_access_unit(
            pair.host,
            idr.data(),
            static_cast<int>(idr.size()),
            10'000
        ),
        MELLO_OK
    );

    // Overflow by volume, not by frame count. This previously sent exactly
    // kMaxQueuedAccessUnits (16) tiny frames, so reaching the bound depended on
    // the burst outrunning the pacer — it passed or failed on scheduling luck.
    // 24 x 512 KB is 12 MB against a 4 MB queue, so the bound is crossed even
    // if most of the burst drains.
    for (uint8_t index = 0; index < 24; ++index) {
        const auto delta = make_large_delta_access_unit(512 * 1024);
        (void)mello_peer_video_send_access_unit(
            pair.host,
            delta.data(),
            static_cast<int>(delta.size()),
            10'000 + static_cast<uint64_t>(index) * 1'000
        );
    }

    // Drain the queue and look for the IDR request, rather than asserting on
    // whichever feedback happens to be first. A burst this size also produces
    // a REMB, and which lands first is not something the sender guarantees.
    bool saw_local_idr_needed = false;
    MelloPeerVideoFeedback feedback{};
    for (int drained = 0; drained < 32; ++drained) {
        if (mello_peer_video_take_feedback(pair.host, &feedback) != 1u) {
            break;
        }
        if (feedback.type == MELLO_PEER_VIDEO_FEEDBACK_LOCAL_IDR_NEEDED) {
            saw_local_idr_needed = true;
            break;
        }
    }
    EXPECT_TRUE(saw_local_idr_needed)
        << "a burst that overflows the send queue must request a local IDR";
}

TEST_F(PeerRtpRolesTest, StatsExposeSenderAndReceiverFields) {
    auto pair = negotiate_host_offer();
    ASSERT_NE(pair.host, nullptr);
    ASSERT_NE(pair.viewer, nullptr);
    ASSERT_TRUE(exchange_ice(pair.host, pair.viewer));
    ASSERT_TRUE(wait_stream_video_ready(pair.host, pair.viewer));

    const auto idr = make_idr_access_unit();
    ASSERT_EQ(
        mello_peer_video_send_access_unit(
            pair.host,
            idr.data(),
            static_cast<int>(idr.size()),
            20'000
        ),
        MELLO_OK
    );

    MelloRtpVideoStats host_stats{};
    mello_peer_video_get_stats(pair.host, &host_stats);
    EXPECT_EQ(host_stats.media_role, MELLO_PEER_MEDIA_ROLE_STREAM_HOST);
    EXPECT_EQ(host_stats.tx_active, 1u);
    EXPECT_GE(host_stats.tx_access_units_enqueued, 1u);

    // Wait on the counter this test asserts, not merely on a successful
    // receive. The two are not simultaneous: rx_core_emitted_access_units is
    // published after the access unit is handed out, so waiting for the recv
    // and then reading stats could observe 0 and fail — intermittently, and
    // only under CI load.
    MelloRtpVideoStats viewer_stats{};
    ASSERT_TRUE(wait_until(
        [&]() {
            std::vector<uint8_t> buffer(256 * 1024);
            (void)mello_peer_video_recv_access_unit(
                pair.viewer,
                buffer.data(),
                static_cast<int>(buffer.size()),
                nullptr
            );
            viewer_stats = MelloRtpVideoStats{};
            mello_peer_video_get_stats(pair.viewer, &viewer_stats);
            return viewer_stats.rx_core_emitted_access_units >= 1u;
        },
        5s
    ));

    mello_peer_video_get_stats(pair.viewer, &viewer_stats);
    EXPECT_EQ(viewer_stats.media_role, MELLO_PEER_MEDIA_ROLE_STREAM_VIEWER);
    EXPECT_EQ(viewer_stats.rx_active, 1u);
    EXPECT_GE(viewer_stats.rx_core_emitted_access_units, 1u);
}

TEST_F(PeerRtpRolesTest, DestroyAfterFailedNegotiationIsSafe) {
    auto* host = mello_peer_create_for_role(
        nullptr,
        "host",
        MELLO_PEER_MEDIA_ROLE_STREAM_HOST
    );
    ASSERT_NE(host, nullptr);
    mello_peer_set_ice_servers(host, nullptr, 0);
    const char* offer = mello_peer_create_offer(host);
    ASSERT_NE(offer, nullptr);

    auto* voice = mello_peer_create(nullptr, "voice");
    ASSERT_NE(voice, nullptr);
    mello_peer_set_ice_servers(voice, nullptr, 0);
    EXPECT_EQ(mello_peer_create_answer(voice, offer), nullptr);

    mello_peer_destroy(voice);
    mello_peer_destroy(host);
}

TEST_F(PeerRtpRolesTest, DestroyAfterConnectedStreamIsSafe) {
    auto pair = negotiate_host_offer();
    ASSERT_NE(pair.host, nullptr);
    ASSERT_NE(pair.viewer, nullptr);
    ASSERT_TRUE(exchange_ice(pair.host, pair.viewer));
    ASSERT_TRUE(wait_stream_video_ready(pair.host, pair.viewer));

    const auto idr = make_idr_access_unit();
    mello_peer_video_send_access_unit(
        pair.host,
        idr.data(),
        static_cast<int>(idr.size()),
        30'000
    );
}

} // namespace
