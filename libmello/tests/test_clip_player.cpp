// The game clip player (plans/GAME-CLIPS.md 6.2): an MP4 that plays from
// its first bytes, the picture on the sound's clock.
//
// The clip is made here with the game clip mux from the H.264 fixture of
// test_replay_mux.cpp (160x96, 30 fps, 2 s) and a sound of 2 s. The demux is
// the real Media Foundation one. A fake decoder stands in for the hardware:
// each frame decodes to a grey picture one step brighter than the last, so
// the CPU frame callback tells which frame is on screen. A thread takes the
// sound 10 ms at a time, as the audio render thread does.
#ifdef _WIN32
#include <gtest/gtest.h>

#include "audio/clip_audio_stream.hpp"
#include "clip/clip_player.hpp"
#include "replay/h264_nal.hpp"
#include "replay/replay_audio_mix.hpp"
#include "replay/replay_mux.hpp"
#include "replay/replay_video_ring.hpp"
#include "video/decoder.hpp"
#include "video/graphics_device.hpp"

#include <d3d11.h>
#include <wrl/client.h>

#include <algorithm>
#include <array>
#include <cstdint>
#include <cstring>
#include <atomic>
#include <chrono>
#include <filesystem>
#include <fstream>
#include <mutex>
#include <thread>
#include <vector>

using namespace mello::replay;
using mello::audio::ClipAudioStream;
using mello::clip::ClipPlayer;
using mello::clip::ClipPlayerOptions;
using mello::clip::ClipPlayerState;
using Microsoft::WRL::ComPtr;
namespace fs = std::filesystem;

namespace {

class TempDir {
public:
    TempDir() {
        const auto stamp = std::chrono::steady_clock::now().time_since_epoch().count();
        path_ = fs::temp_directory_path() / ("mello_clip_player_test_" + std::to_string(stamp));
        fs::create_directories(path_);
    }
    ~TempDir() {
        std::error_code ec;
        fs::remove_all(path_, ec);
    }
    const fs::path& path() const { return path_; }

private:
    fs::path path_;
};

std::vector<std::vector<uint8_t>> fixture_units() {
    const fs::path file = fs::path(MELLO_REPLAY_FIXTURE_DIR) / "gop30_160x96.h264";
    std::ifstream f(file, std::ios::binary);
    std::vector<uint8_t> all((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
    std::vector<std::vector<uint8_t>> units;
    h264::for_each_nal(all.data(), all.size(), [&](uint8_t type, const uint8_t* nal, size_t size) {
        if (type == h264::kNalAud || units.empty()) units.emplace_back();
        static const uint8_t kStart[] = {0, 0, 0, 1};
        units.back().insert(units.back().end(), kStart, kStart + 4);
        units.back().insert(units.back().end(), nal, nal + size);
    });
    return units;
}

constexpr uint64_t kT0 = 50'000'000;
constexpr uint64_t kFrameUs = 33'333;

/// A 2 s clip: the 60 fixture frames at 30 fps, and 2 s of a quiet tone.
std::vector<uint8_t> make_clip(const TempDir& dir) {
    const auto units = fixture_units();
    ReplayVideoRing ring;
    ReplayVideoRing::Config c;
    c.dir = dir.path();
    c.segment_bytes = 64 * 1024;
    c.writer_thread = false;
    if (!ring.start(c)) return {};
    for (size_t i = 0; i < units.size(); ++i) {
        auto copy = units[i];
        ring.push(std::move(copy), kT0 + i * kFrameUs, h264::has_idr(units[i].data(), units[i].size()));
    }
    ring.flush();
    const uint64_t end = kT0 + units.size() * kFrameUs;
    auto window = ring.pin_window(kT0, end);

    ClipAudioSource game;
    game.channels = 2;
    for (uint64_t f = 0; f < 96000; f += 480) {
        TimedPcm chunk;
        chunk.capture_us = kT0 + f * 1'000'000 / 48000;
        chunk.samples.assign(960, 2000);
        game.chunks.push_back(std::move(chunk));
    }
    ClipMuxInput in;
    in.units = window.units();
    in.read_unit = [&](const AccessUnitRef& u, std::vector<uint8_t>& out) { return window.read(u, out); };
    in.width = 160;
    in.height = 96;
    in.fps = 30;
    in.end_us = end;
    in.audio = mix_clip_audio(kT0, end, {game});
    const fs::path mp4 = dir.path() / "clip.mp4";
    if (!write_clip_mp4(in, mp4)) return {};
    std::ifstream f(mp4, std::ios::binary);
    return std::vector<uint8_t>((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
}

/// Each frame decodes to a grey 160x96 picture, 3 steps brighter than the
/// one before.
class StepDecoder : public mello::video::Decoder {
public:
    explicit StepDecoder(ID3D11Device* device) : device_(device) { device_->GetImmediateContext(&context_); }
    bool initialize(const mello::video::GraphicsDevice&, const mello::video::DecoderConfig&) override {
        return true;
    }
    void shutdown() override {}
    mello::video::DecodeFeedResult decode(const uint8_t*, size_t size, bool, int slot) override {
        if (size == 0) return mello::video::DecodeFeedResult::Error;
        Slot& s = slots_[slot];
        if (!s.texture) {
            D3D11_TEXTURE2D_DESC desc{};
            desc.Width = 160;
            desc.Height = 96;
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            desc.Format = DXGI_FORMAT_NV12;
            desc.SampleDesc.Count = 1;
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
            if (FAILED(device_->CreateTexture2D(&desc, nullptr, &s.texture))) {
                return mello::video::DecodeFeedResult::Error;
            }
        }
        const uint8_t luma = static_cast<uint8_t>(20 + (count_ % 70) * 3);
        ++count_;
        std::vector<uint8_t> nv12(160 * 96 * 3 / 2, 128);
        std::fill(nv12.begin(), nv12.begin() + 160 * 96, luma);
        context_->UpdateSubresource(s.texture.Get(), 0, nullptr, nv12.data(), 160, 0);
        return mello::video::DecodeFeedResult::FrameReady;
    }
    bool picture(int slot, mello::video::DecodedPicture& out) const override {
        const Slot& s = slots_[slot];
        if (!s.texture) return false;
        out.texture = s.texture;
        out.format = DXGI_FORMAT_NV12;
        out.width = 160;
        out.height = 96;
        return true;
    }
    bool supports_codec(mello::video::VideoCodec) const override { return true; }
    const char* name() const override { return "Step"; }

private:
    struct Slot {
        ComPtr<ID3D11Texture2D> texture;
    };
    ID3D11Device* device_;
    ComPtr<ID3D11DeviceContext> context_;
    Slot slots_[kOutputSlots];
    int count_ = 0;
};

int luma_of(const uint8_t* rgba) { return static_cast<int>(rgba[1] * 219.0 / 255.0 + 16.0 + 0.5); }

struct Shown {
    int64_t at_us;
    int luma;
};

/// The player around one clip: the decoder, the frames on screen, and a
/// thread that takes the sound 10 ms at a time.
class PlayerRig {
public:
    PlayerRig(const std::vector<uint8_t>& mp4, std::function<int64_t(uint64_t)> frontier = {})
        : mp4_(mp4), frontier_(std::move(frontier)) {
        device_ = mello::video::create_d3d11_device();
        ClipPlayerOptions o;
        o.length = mp4_.size();
        o.read = [this](uint64_t offset, uint8_t* buf, uint32_t len) -> int64_t {
            if (offset >= mp4_.size()) return 0;
            if (frontier_ && static_cast<int64_t>(offset) >= frontier_(offset)) return mello::clip::kReadWait;
            const uint64_t n = std::min<uint64_t>(len, mp4_.size() - offset);
            std::memcpy(buf, mp4_.data() + offset, n);
            return static_cast<int64_t>(n);
        };
        o.attach_audio = [this](std::shared_ptr<ClipAudioStream> s) {
            std::lock_guard<std::mutex> lock(mutex_);
            audio_ = std::move(s);
        };
        o.output_latency_us = [] { return int64_t{20'000}; };
        o.make_decoder = [](const mello::video::GraphicsDevice& device) {
            return std::make_unique<StepDecoder>(device.d3d11());
        };
        o.on_cpu_frame = [this](const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts) {
            std::lock_guard<std::mutex> lock(mutex_);
            shown_.push_back({static_cast<int64_t>(ts), luma_of(rgba + (h / 2 * w + w / 2) * 4)});
        };
        render_ = std::thread([this] { render_main(); });
        player_ = ClipPlayer::start(std::move(o));
    }
    ~PlayerRig() {
        if (player_) player_->stop();
        running_ = false;
        if (render_.joinable()) render_.join();
        if (device_.handle) device_.d3d11()->Release();
    }

    ClipPlayer& player() { return *player_; }
    bool valid() const { return device_.handle && player_; }

    /// Waits until `done` holds or `ms` pass. Records whether the state ever
    /// said buffering.
    bool wait_for(const std::function<bool(const ClipPlayerState&)>& done, int ms) {
        const auto until = std::chrono::steady_clock::now() + std::chrono::milliseconds(ms);
        while (std::chrono::steady_clock::now() < until) {
            const ClipPlayerState s = player_->state();
            if (s.buffering) saw_buffering = true;
            if (done(s)) return true;
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
        return false;
    }

    std::vector<Shown> shown() {
        std::lock_guard<std::mutex> lock(mutex_);
        return shown_;
    }
    uint64_t sound_taken() const { return sound_taken_.load(); }

    bool saw_buffering = false;

private:
    void render_main() {
        std::array<int16_t, 960> stereo{};
        std::array<int16_t, 480> mono{};
        auto next = std::chrono::steady_clock::now();
        while (running_) {
            next += std::chrono::milliseconds(10);
            std::this_thread::sleep_until(next);
            std::shared_ptr<ClipAudioStream> s;
            {
                std::lock_guard<std::mutex> lock(mutex_);
                s = audio_;
            }
            if (!s) continue;
            stereo.fill(0);
            const uint64_t before = s->played_frames();
            s->mix_into(stereo.data(), mono.data(), 480, mello::video::ViewerPipeline::now_us());
            sound_taken_ += s->played_frames() - before;
        }
    }

    const std::vector<uint8_t>& mp4_;
    std::function<int64_t(uint64_t)> frontier_;
    mello::video::GraphicsDevice device_{};
    std::unique_ptr<ClipPlayer> player_;
    std::mutex mutex_;
    std::shared_ptr<ClipAudioStream> audio_;
    std::vector<Shown> shown_;
    std::atomic<bool> running_{true};
    std::atomic<uint64_t> sound_taken_{0};
    std::thread render_;
};

}  // namespace

// The whole clip plays in order, at the sound's pace, and ends.
TEST(ClipPlayer, AClipPlaysInOrderOnTheClockOfItsSound) {
    TempDir dir;
    const auto mp4 = make_clip(dir);
    ASSERT_FALSE(mp4.empty()) << "no clip made";
    PlayerRig rig(mp4);
    if (!rig.valid()) GTEST_SKIP() << "no D3D11 device";

    ASSERT_TRUE(rig.wait_for([](const ClipPlayerState& s) { return s.ended || s.failed; }, 8000));
    const ClipPlayerState s = rig.player().state();
    EXPECT_FALSE(s.failed);
    EXPECT_EQ(s.width, 160u);
    EXPECT_EQ(s.height, 96u);
    EXPECT_NEAR(static_cast<double>(s.duration_us) / 1e6, 2.0, 0.1);

    const auto shown = rig.shown();
    ASSERT_GE(shown.size(), 50u) << "60 frames, a few may drop";
    for (size_t i = 1; i < shown.size(); ++i) {
        EXPECT_GT(shown[i].luma, shown[i - 1].luma) << "frame " << i << " out of order";
    }
    const double span_s = static_cast<double>(shown.back().at_us - shown.front().at_us) / 1e6;
    EXPECT_NEAR(span_s, 1.97, 0.25) << "the frames follow the 2 s of sound";
    EXPECT_NEAR(static_cast<double>(rig.sound_taken()) / 48000.0, 2.0, 0.1) << "the whole sound played";
}

// A read that waits holds the picture and the sound and says buffering;
// play goes on when the bytes come.
TEST(ClipPlayer, AStallHoldsThePictureAndSaysBuffering) {
    TempDir dir;
    const auto mp4 = make_clip(dir);
    ASSERT_FALSE(mp4.empty());
    const auto t0 = std::chrono::steady_clock::now();
    // The first half of the file at once, the rest after 1.5 s.
    const int64_t half = static_cast<int64_t>(mp4.size() / 2);
    PlayerRig rig(mp4, [t0, half](uint64_t) -> int64_t {
        const auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - t0);
        return ms.count() < 1500 ? half : INT64_MAX;
    });
    if (!rig.valid()) GTEST_SKIP() << "no D3D11 device";

    ASSERT_TRUE(rig.wait_for([](const ClipPlayerState& s) { return s.ended || s.failed; }, 10000));
    EXPECT_FALSE(rig.player().state().failed);
    EXPECT_TRUE(rig.saw_buffering) << "the stall never showed";
    EXPECT_GE(rig.shown().size(), 50u) << "the clip played to its end after the stall";
}

// A seek plays from the target: the clock jumps there and the clip ends
// sooner than from the start.
TEST(ClipPlayer, ASeekPlaysFromTheTarget) {
    TempDir dir;
    const auto mp4 = make_clip(dir);
    ASSERT_FALSE(mp4.empty());
    PlayerRig rig(mp4);
    if (!rig.valid()) GTEST_SKIP() << "no D3D11 device";

    ASSERT_TRUE(rig.wait_for([](const ClipPlayerState& s) { return s.position_us > 200'000; }, 4000));
    const auto at_seek = std::chrono::steady_clock::now();
    rig.player().seek(1'500'000);
    ASSERT_TRUE(rig.wait_for([](const ClipPlayerState& s) { return s.position_us >= 1'450'000; }, 2000))
        << "the clock did not jump to the target";
    ASSERT_TRUE(rig.wait_for([](const ClipPlayerState& s) { return s.ended || s.failed; }, 4000));
    const double after_s =
        std::chrono::duration<double>(std::chrono::steady_clock::now() - at_seek).count();
    EXPECT_LT(after_s, 1.4) << "it played on from the target, not from where it was";
}

// A pause stops the clock and the picture; play goes on from there.
TEST(ClipPlayer, APauseHoldsTheClock) {
    TempDir dir;
    const auto mp4 = make_clip(dir);
    ASSERT_FALSE(mp4.empty());
    PlayerRig rig(mp4);
    if (!rig.valid()) GTEST_SKIP() << "no D3D11 device";

    ASSERT_TRUE(rig.wait_for([](const ClipPlayerState& s) { return s.position_us > 500'000; }, 4000));
    rig.player().set_paused(true);
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
    const int64_t held = rig.player().state().position_us;
    const size_t shown_held = rig.shown().size();
    std::this_thread::sleep_for(std::chrono::milliseconds(400));
    EXPECT_TRUE(rig.player().state().paused);
    EXPECT_NEAR(static_cast<double>(rig.player().state().position_us), static_cast<double>(held), 15'000.0);
    EXPECT_LE(rig.shown().size(), shown_held + 2) << "the picture went on while paused";

    rig.player().set_paused(false);
    ASSERT_TRUE(rig.wait_for([](const ClipPlayerState& s) { return s.ended || s.failed; }, 4000));
    EXPECT_FALSE(rig.player().state().failed);
}

#endif  // _WIN32
