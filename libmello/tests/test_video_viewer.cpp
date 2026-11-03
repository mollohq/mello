// The stream viewer pipeline of streaming-quality stage 3: decode thread,
// 3-slot ring, present thread, converter with the keyed-mutex shared texture.
//
// A fake decoder stands in for the hardware: each access unit is
// [luma value, width / 16, height / 16] and "decodes" to an NV12 picture of
// that colour and size, written into the slot texture the pipeline names. So
// every case runs on any D3D11 adapter, also the WARP software adapter, with
// no codec.

#include <gtest/gtest.h>

#ifdef _WIN32
#include "transport/video_receive_timeline.hpp"
#include "video/graphics_device.hpp"
#include "video/staging_texture.hpp"
#include "video/video_viewer.hpp"

#include <d3d11_1.h>
#include <dxgi1_2.h>
#include <wrl/client.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <thread>
#include <vector>

using namespace mello::video;
using mello::transport::TimelineInput;
using mello::transport::VideoReceiveTimeline;
using Microsoft::WRL::ComPtr;

namespace {

struct TestDevice {
    GraphicsDevice device = create_d3d11_device();
    ~TestDevice() {
        if (device.handle) device.d3d11()->Release();
    }
};

bool nv12_shader_input_supported(ID3D11Device* device) {
    UINT support = 0;
    if (FAILED(device->CheckFormatSupport(DXGI_FORMAT_NV12, &support))) return false;
    return (support & D3D11_FORMAT_SUPPORT_TEXTURE2D) && (support & D3D11_FORMAT_SUPPORT_SHADER_LOAD);
}

class FakeDecoder : public Decoder {
public:
    explicit FakeDecoder(ID3D11Device* device) : device_(device) { device_->GetImmediateContext(&context_); }

    bool initialize(const GraphicsDevice&, const DecoderConfig&) override { return true; }
    void shutdown() override {}

    DecodeFeedResult decode(const uint8_t* data, size_t size, bool, int slot) override {
        if (size < 3) return DecodeFeedResult::Error;
        const uint32_t w = data[1] * 16u;
        const uint32_t h = data[2] * 16u;
        Slot& s = slots_[slot];
        if (!s.texture || s.width != w || s.height != h) {
            D3D11_TEXTURE2D_DESC desc{};
            desc.Width = w;
            desc.Height = h;
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            desc.Format = DXGI_FORMAT_NV12;
            desc.SampleDesc.Count = 1;
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
            s.texture.Reset();
            if (FAILED(device_->CreateTexture2D(&desc, nullptr, &s.texture))) return DecodeFeedResult::Error;
            s.width = w;
            s.height = h;
        }
        std::vector<uint8_t> nv12(static_cast<size_t>(w) * h * 3 / 2, 128);
        std::fill(nv12.begin(), nv12.begin() + static_cast<size_t>(w) * h, data[0]);
        context_->UpdateSubresource(s.texture.Get(), 0, nullptr, nv12.data(), w, 0);
        ++decoded;
        return DecodeFeedResult::FrameReady;
    }

    bool picture(int slot, DecodedPicture& out) const override {
        const Slot& s = slots_[slot];
        if (!s.texture) return false;
        out.texture = s.texture;
        out.format = DXGI_FORMAT_NV12;
        out.width = s.width;
        out.height = s.height;
        return true;
    }

    bool supports_codec(VideoCodec) const override { return true; }
    const char* name() const override { return "Fake"; }

    std::atomic<int> decoded{0};

private:
    struct Slot {
        ComPtr<ID3D11Texture2D> texture;
        uint32_t width = 0;
        uint32_t height = 0;
    };
    ID3D11Device* device_;
    ComPtr<ID3D11DeviceContext> context_;
    Slot slots_[kOutputSlots];
};

// A frame that goes in more than half a frame interval late (60 fps).
constexpr int64_t kFeedLateUs = 8'000;

std::vector<uint8_t> unit(uint8_t luma, uint8_t w16 = 4, uint8_t h16 = 2) { return {luma, w16, h16}; }

// Waits for `done`, up to `limit`. The pipeline threads can start late: on a
// loaded machine (the self-hosted runner while other builds run) a new thread
// waited seconds for its first time slice, and a fixed sleep then saw nothing
// decoded or shown.
template <typename Done>
bool wait_for(Done done, std::chrono::milliseconds limit = std::chrono::seconds(10)) {
    const auto end = std::chrono::steady_clock::now() + limit;
    while (!done()) {
        if (std::chrono::steady_clock::now() >= end) return false;
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    return true;
}

// Luma back from an RGBA pixel of a grey picture (U = V = 128).
int luma_of(const uint8_t* rgba) { return static_cast<int>(rgba[1] * 219.0 / 255.0 + 16.0 + 0.5); }

struct Present {
    int64_t at_us;
    int luma;
    uint32_t w;
};

class ViewerPipelineTest : public ::testing::Test {
protected:
    void SetUp() override {
        if (!test.device.handle) GTEST_SKIP() << "no D3D11 device";
        if (!nv12_shader_input_supported(test.device.d3d11())) GTEST_SKIP() << "no NV12 shader input";
    }

    bool start(bool cpu = true) {
        auto decoder = std::make_unique<FakeDecoder>(test.device.d3d11());
        fake = decoder.get();
        ViewerPipeline::FrameCallback cb;
        if (cpu) {
            cb = [this](const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts) {
                std::lock_guard<std::mutex> lock(mutex);
                presents.push_back({static_cast<int64_t>(ts), luma_of(rgba + (h / 2 * w + w / 2) * 4), w});
            };
        }
        ViewerConfig config;
        config.width = 64;
        config.height = 32;
        return viewer.start(test.device, config, std::move(cb), std::move(decoder));
    }

    std::vector<Present> snapshot() {
        std::lock_guard<std::mutex> lock(mutex);
        return presents;
    }

    // One frame through decode and present, so both pipeline threads run
    // before a case times anything. The frame is then forgotten.
    bool warm_up() {
        if (!viewer.feed_packet(unit(16).data(), 3, true)) return false;
        const bool shown = wait_for([this] { return !snapshot().empty(); });
        std::lock_guard<std::mutex> lock(mutex);
        presents.clear();
        return shown;
    }

    uint64_t presented_or_dropped() {
        ViewerStats stats;
        viewer.stats(stats);
        return stats.frames_presented + stats.frames_dropped_present;
    }

    TestDevice test;
    ViewerPipeline viewer;
    FakeDecoder* fake = nullptr;
    std::mutex mutex;
    std::vector<Present> presents;
};

// The present thread shows each released frame at its render time: one
// present per frame, at the 60 fps cadence, in order. The client before
// stage 3 presented on a 16 ms tick that beat against the frame rate (frame
// interval p95 32.8 ms on clean).
TEST_F(ViewerPipelineTest, FramesFromTheTimelineAreShownAtTheirRenderTime) {
    ASSERT_TRUE(start());
    ASSERT_TRUE(warm_up());
    auto timeline = std::make_shared<VideoReceiveTimeline>();
    viewer.attach_timeline([timeline] { return timeline; });

    constexpr int kFrames = 90;
    // A cadence needs a machine that runs this thread on time. One late frame
    // is a hiccup; more than 1 in 20 and the case measures the machine, not
    // the pipeline.
    int late_frames = 0;
    int64_t max_feed_late_us = 0;
    const auto t0 = std::chrono::steady_clock::now();
    for (int i = 0; i < kFrames; ++i) {
        const auto due = t0 + std::chrono::microseconds(16'667 * i);
        std::this_thread::sleep_until(due);
        const int64_t late_us =
            std::chrono::duration_cast<std::chrono::microseconds>(std::chrono::steady_clock::now() - due).count();
        if (late_us > kFeedLateUs) ++late_frames;
        max_feed_late_us = std::max<int64_t>(max_feed_late_us, late_us);
        TimelineInput in;
        in.id = i;
        in.reference = i - 1;
        in.keyframe = i == 0;
        in.rtp_timestamp = static_cast<uint32_t>(i * 1500);
        in.arrival_us = ViewerPipeline::now_us();
        in.annex_b = unit(static_cast<uint8_t>(20 + i * 2));
        timeline->insert(std::move(in));
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    viewer.stop();

    if (late_frames > kFrames / 20) {
        GTEST_SKIP() << "the machine is too loaded to measure a cadence: " << late_frames << " of " << kFrames
                     << " frames went in late, up to " << max_feed_late_us / 1000 << " ms";
    }
    const auto shown = snapshot();
    ASSERT_GE(shown.size(), static_cast<size_t>(kFrames - 6));
    std::vector<double> intervals;
    for (size_t i = 1; i < shown.size(); ++i) {
        EXPECT_GT(shown[i].luma, shown[i - 1].luma) << "in order, never twice";
        // The first half second is the delay settling (VCMTiming moves the
        // delay at most 100 ms per second): not the cadence.
        if (i > 30) intervals.push_back((shown[i].at_us - shown[i - 1].at_us) / 1000.0);
    }
    std::sort(intervals.begin(), intervals.end());
    const double p50 = intervals[intervals.size() / 2];
    const double p95 = intervals[intervals.size() * 95 / 100];
    EXPECT_NEAR(p50, 16.7, 2.0);
    // The gate's limit is 20 ms (measured here at 16 to 18). The bound keeps
    // room for a loaded test machine and still fails the 16 ms tick that
    // beat against the frame rate (p95 32.8 ms).
    EXPECT_LT(p95, 25.0) << "frame interval p95";
    ViewerStats stats;
    viewer.stats(stats);
    EXPECT_EQ(stats.timeline.keyframe_requests, 0u);
}

// Frames that are all due at once (a burst after a stall): only the newest
// is shown, the others count as dropped, nothing waits.
TEST_F(ViewerPipelineTest, OnlyTheNewestDueFrameIsShown) {
    ASSERT_TRUE(start());
    // Without a timeline every fed frame is due at once.
    for (uint8_t v : {40, 60, 80, 100, 120}) {
        ASSERT_TRUE(viewer.feed_packet(unit(v).data(), 3, v == 40));
    }
    const bool decoded = wait_for([this] { return fake->decoded.load() >= 5; });
    const bool settled = wait_for([this] { return presented_or_dropped() >= 5; });
    viewer.stop();
    ASSERT_TRUE(decoded);
    ASSERT_TRUE(settled);
    const auto shown = snapshot();
    ASSERT_FALSE(shown.empty());
    EXPECT_NEAR(shown.back().luma, 120, 2) << "the newest frame is on screen";
    ViewerStats stats;
    viewer.stats(stats);
    EXPECT_EQ(stats.frames_presented + stats.frames_dropped_present, 5u);
}

// A frame source that releases the frames given to it, each at its render
// time (a game clip's source, in short).
class ScriptedSource : public mello::transport::FrameSource {
public:
    void push(std::vector<uint8_t> unit, int64_t render_us) {
        {
            std::lock_guard<std::mutex> lock(m_);
            mello::transport::TimelineFrame f;
            f.annex_b = std::move(unit);
            f.keyframe = frames_.empty();
            f.render_time_us = render_us;
            frames_.push_back(std::move(f));
        }
        cv_.notify_all();
    }
    mello::transport::TimelineDecision poll() override {
        std::lock_guard<std::mutex> lock(m_);
        mello::transport::TimelineDecision d;
        if (frames_.empty()) return d;
        d.kind = mello::transport::TimelineDecision::Kind::Release;
        d.frame = std::move(frames_.front());
        frames_.pop_front();
        return d;
    }
    void on_decoded(int64_t) override {}
    void on_decode_error() override {}
    bool wait(int64_t wake_us) override {
        std::unique_lock<std::mutex> lock(m_);
        const int64_t ms = wake_us > 0 ? std::max<int64_t>(0, (wake_us - ViewerPipeline::now_us()) / 1000) : 100;
        cv_.wait_for(lock, std::chrono::milliseconds(ms), [this] { return !frames_.empty(); });
        return true;
    }
    void wake() override { cv_.notify_all(); }
    mello::transport::TimelineStats stats() const override { return {}; }

private:
    std::mutex m_;
    std::condition_variable cv_;
    std::deque<mello::transport::TimelineFrame> frames_;
};

// A clip seek: the frames that wait in the ring for their render time are
// dropped at a flush and never shown.
TEST_F(ViewerPipelineTest, AFlushDropsTheFramesThatWaitInTheRing) {
    ASSERT_TRUE(start());
    auto source = std::make_shared<ScriptedSource>();
    const int64_t later = ViewerPipeline::now_us() + 400'000;
    source->push(unit(40), later);
    source->push(unit(60), later + 16'667);
    viewer.attach_timeline([source] { return source; });
    for (int i = 0; i < 200 && fake->decoded.load() < 2; ++i) {
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    ASSERT_EQ(fake->decoded.load(), 2);
    EXPECT_EQ(viewer.ring_depth(), 2u);

    viewer.flush();
    EXPECT_EQ(viewer.ring_depth(), 0u);
    std::this_thread::sleep_for(std::chrono::milliseconds(600));
    viewer.stop();
    EXPECT_TRUE(snapshot().empty()) << "a flushed frame was shown";
}

// A new picture size (a new SPS) gets new slot textures and a new shared
// texture: the presenter sees it with the new size.
TEST_F(ViewerPipelineTest, ASizeChangeGivesANewSharedTexture) {
    ASSERT_TRUE(start(false));
    std::mutex m;
    std::vector<std::tuple<void*, uint32_t, uint32_t>> handles;
    viewer.set_native_frame_callback([&](void* handle, uint32_t w, uint32_t h, uint32_t format, uint32_t, uint64_t) {
        EXPECT_EQ(format, 1u) << "RGBA8";
        std::lock_guard<std::mutex> lock(m);
        handles.emplace_back(handle, w, h);
    });
    auto handle_count = [&] {
        std::lock_guard<std::mutex> lock(m);
        return handles.size();
    };
    // Each size is on screen before the next comes in.
    const bool fed = viewer.feed_packet(unit(50, 4, 2).data(), 3, true) &&
                     wait_for([&] { return handle_count() >= 1; }) &&
                     viewer.feed_packet(unit(60, 6, 3).data(), 3, true) &&
                     wait_for([&] { return handle_count() >= 2; });
    viewer.stop();
    ASSERT_TRUE(fed);
    std::lock_guard<std::mutex> lock(m);
    ASSERT_EQ(handles.size(), 2u);
    EXPECT_EQ(std::get<1>(handles[0]), 64u);
    EXPECT_EQ(std::get<1>(handles[1]), 96u);
    EXPECT_EQ(std::get<2>(handles[1]), 48u);
    // The presenter opens the texture again when the handle or the size
    // changes (dcomp_presenter.rs). The handle value alone can come back: the
    // old NT handle is closed before the new one is made.
    EXPECT_NE(handles[0], handles[1]);
}

// The presenter's side of the contract: the shared texture opens on another
// device (the client's DComp device) and is read under keyed-mutex key 0.
TEST(ViewerConverter, TheSharedFrameIsReadUnderTheKeyedMutex) {
    TestDevice test;
    if (!test.device.handle) GTEST_SKIP() << "no D3D11 device";
    if (!nv12_shader_input_supported(test.device.d3d11())) GTEST_SKIP() << "no NV12 shader input";
    FakeDecoder decoder(test.device.d3d11());
    const auto au = unit(180, 4, 2);
    ASSERT_EQ(decoder.decode(au.data(), au.size(), true, 0), DecodeFeedResult::FrameReady);
    DecodedPicture picture;
    ASSERT_TRUE(decoder.picture(0, picture));

    StagingTexture converter;
    ASSERT_TRUE(converter.initialize(test.device, false));
    ASSERT_TRUE(converter.convert(picture, false));
    ASSERT_NE(converter.shared_rgba_handle(), nullptr);

    // A second device, as the presenter has.
    ComPtr<ID3D11Device> other;
    ComPtr<ID3D11DeviceContext> other_ctx;
    ComPtr<IDXGIDevice> dxgi;
    ComPtr<IDXGIAdapter> adapter;
    ASSERT_TRUE(SUCCEEDED(test.device.d3d11()->QueryInterface(IID_PPV_ARGS(&dxgi))));
    ASSERT_TRUE(SUCCEEDED(dxgi->GetAdapter(&adapter)));
    ASSERT_TRUE(SUCCEEDED(D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, nullptr, 0,
                                            D3D11_SDK_VERSION, &other, nullptr, &other_ctx)));
    ComPtr<ID3D11Device1> other1;
    ASSERT_TRUE(SUCCEEDED(other.As(&other1)));
    ComPtr<ID3D11Texture2D> opened;
    ASSERT_TRUE(SUCCEEDED(other1->OpenSharedResource1(static_cast<HANDLE>(converter.shared_rgba_handle()),
                                                      IID_PPV_ARGS(&opened))));
    ComPtr<IDXGIKeyedMutex> mutex;
    ASSERT_TRUE(SUCCEEDED(opened.As(&mutex))) << "the shared frame has a keyed mutex";
    ASSERT_EQ(mutex->AcquireSync(0, 1000), S_OK);

    D3D11_TEXTURE2D_DESC desc{};
    opened->GetDesc(&desc);
    desc.BindFlags = 0;
    desc.MiscFlags = 0;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ComPtr<ID3D11Texture2D> staging;
    ASSERT_TRUE(SUCCEEDED(other->CreateTexture2D(&desc, nullptr, &staging)));
    other_ctx->CopyResource(staging.Get(), opened.Get());
    mutex->ReleaseSync(0);
    D3D11_MAPPED_SUBRESOURCE mapped{};
    ASSERT_TRUE(SUCCEEDED(other_ctx->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped)));
    const auto* px = static_cast<const uint8_t*>(mapped.pData) + mapped.RowPitch * 16 + 32 * 4;
    EXPECT_NEAR(luma_of(px), 180, 2);
    other_ctx->Unmap(staging.Get(), 0);
    converter.shutdown();
}

// NVDEC's R8 texture with the NV12 layout converts as NV12 does.
TEST(ViewerConverter, TheR8LayoutConvertsWithItsChromaRow) {
    TestDevice test;
    if (!test.device.handle) GTEST_SKIP() << "no D3D11 device";
    constexpr uint32_t kW = 64, kH = 32, kRows = kH + kH / 2;
    std::vector<uint8_t> data(static_cast<size_t>(kW) * kRows, 128);
    std::fill(data.begin(), data.begin() + kW * kH, uint8_t{90});
    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = kW;
    desc.Height = kRows;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_R8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    D3D11_SUBRESOURCE_DATA init{data.data(), kW, 0};
    DecodedPicture picture;
    ASSERT_TRUE(SUCCEEDED(test.device.d3d11()->CreateTexture2D(&desc, &init, &picture.texture)));
    picture.format = DXGI_FORMAT_R8_UNORM;
    picture.width = kW;
    picture.height = kH;
    picture.uv_row = kH;

    StagingTexture converter;
    ASSERT_TRUE(converter.initialize(test.device, true));
    ASSERT_TRUE(converter.convert(picture, true));
    std::vector<uint8_t> rgba(static_cast<size_t>(kW) * kH * 4);
    ASSERT_TRUE(converter.read_rgba(rgba.data()));
    converter.shutdown();
    EXPECT_NEAR(luma_of(&rgba[(16 * kW + 32) * 4]), 90, 2);
    EXPECT_NEAR(rgba[(16 * kW + 32) * 4 + 0], rgba[(16 * kW + 32) * 4 + 2], 2) << "grey: no chroma shift";
}

} // namespace
#endif // _WIN32
