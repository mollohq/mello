// Windows decoder choice (decoder_factory.cpp), the Media Foundation H.264
// decoder (decoder_mf.cpp) and the viewer's switch to the next decoder on
// Windows (video_viewer.cpp).
//
// The decode tests use the checked-in fixtures in fixtures/video/ (made by
// libmello's OpenH264 encoder, see the README there), so they need no
// encoder and run in the gate. They need a D3D11 device and the Microsoft
// H.264 decoder; a Windows N edition without the Media Feature Pack skips.
#ifdef _WIN32
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <d3d11.h>
#include <wrl/client.h>

#include "replay/h264_nal.hpp"
#include "video/capture_reference.hpp"
#include "video/decoder_factory.hpp"
#include "video/decoder_mf.hpp"
#include "video/graphics_device.hpp"
#include "video/video_pipeline.hpp"
#include "video/video_viewer.hpp"
#include <gtest/gtest.h>

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <string>
#include <thread>
#include <vector>

using Microsoft::WRL::ComPtr;
namespace fs = std::filesystem;

namespace mello::video {
namespace {

constexpr const char* kStepFixture = "bands_640x360_then_320x180.h264";
constexpr const char* k1080pFixture = "bands_1920x1080.h264";
// Each fixture has this many access units at each size.
constexpr size_t kUnitsPerSize = 10;

// Sets an environment variable in the process environment (the decoders
// read it with GetEnvironmentVariableA) and restores the old value.
class EnvVar {
public:
    EnvVar(const char* name, const char* value) : name_(name) {
        char buf[256] = {};
        const DWORD n = GetEnvironmentVariableA(name, buf, sizeof(buf));
        had_ = n > 0 && n < sizeof(buf);
        if (had_) old_.assign(buf, n);
        SetEnvironmentVariableA(name, value);
    }
    ~EnvVar() { SetEnvironmentVariableA(name_.c_str(), had_ ? old_.c_str() : nullptr); }

private:
    std::string name_;
    std::string old_;
    bool had_ = false;
};

struct Units {
    std::vector<std::vector<uint8_t>> aus;
    std::vector<bool> keys;
};

// The fixture's access units. Each one starts with an AUD in the file; the
// AUD is dropped, as the live stream has none.
Units read_units(const char* name) {
    Units u;
    std::ifstream f(fs::path(MELLO_VIDEO_FIXTURE_DIR) / name, std::ios::binary);
    const std::vector<uint8_t> all((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
    replay::h264::for_each_nal(all.data(), all.size(), [&](uint8_t type, const uint8_t* nal, size_t size) {
        if (type == replay::h264::kNalAud) {
            u.aus.emplace_back();
            return;
        }
        if (u.aus.empty()) u.aus.emplace_back();
        static const uint8_t kStart[] = {0, 0, 0, 1};
        u.aus.back().insert(u.aus.back().end(), kStart, kStart + 4);
        u.aus.back().insert(u.aus.back().end(), nal, nal + size);
    });
    for (const auto& au : u.aus) u.keys.push_back(replay::h264::has_idr(au.data(), au.size()));
    return u;
}

// The NV12 planes of a slot texture, read back through a staging texture.
struct Nv12 {
    uint32_t w = 0, h = 0;
    std::vector<uint8_t> y;   // w x h
    std::vector<uint8_t> uv;  // w x h/2, interleaved U and V
    uint8_t luma(uint32_t x, uint32_t yy) const { return y[static_cast<size_t>(yy) * w + x]; }
    uint8_t u(uint32_t x, uint32_t yy) const { return uv[static_cast<size_t>(yy / 2) * w + (x & ~1u)]; }
    uint8_t v(uint32_t x, uint32_t yy) const { return uv[static_cast<size_t>(yy / 2) * w + (x & ~1u) + 1]; }
};

bool read_nv12(ID3D11Device* device, ID3D11Texture2D* texture, Nv12& out) {
    D3D11_TEXTURE2D_DESC desc{};
    texture->GetDesc(&desc);
    if (desc.Format != DXGI_FORMAT_NV12) return false;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    desc.MiscFlags = 0;
    ComPtr<ID3D11Texture2D> staging;
    if (FAILED(device->CreateTexture2D(&desc, nullptr, &staging))) return false;
    ComPtr<ID3D11DeviceContext> ctx;
    device->GetImmediateContext(&ctx);
    ctx->CopyResource(staging.Get(), texture);
    D3D11_MAPPED_SUBRESOURCE m{};
    if (FAILED(ctx->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &m))) return false;
    out.w = desc.Width;
    out.h = desc.Height;
    out.y.resize(static_cast<size_t>(out.w) * out.h);
    out.uv.resize(static_cast<size_t>(out.w) * out.h / 2);
    const auto* base = static_cast<const uint8_t*>(m.pData);
    for (uint32_t r = 0; r < out.h; ++r) {
        std::memcpy(out.y.data() + static_cast<size_t>(r) * out.w, base + static_cast<size_t>(r) * m.RowPitch,
                    out.w);
    }
    // A mapped NV12 texture has its chroma rows after `Height` luma rows.
    const uint8_t* uv = base + static_cast<size_t>(m.RowPitch) * out.h;
    for (uint32_t r = 0; r < out.h / 2; ++r) {
        std::memcpy(out.uv.data() + static_cast<size_t>(r) * out.w, uv + static_cast<size_t>(r) * m.RowPitch,
                    out.w);
    }
    ctx->Unmap(staging.Get(), 0);
    return true;
}

// The fixture picture (README in fixtures/video): luma 60 on the left half;
// on the right half 120 + (frame % 5) x 16, so each picture differs from the
// one before it; chroma U 148 / V 108 in the top 32 rows, U 108 / V 148
// down to the middle, U 148 / V 148 below. Every colour is inside the RGB
// gamut, so the encode path (NV12 to BGRA and back) keeps it. A chroma
// plane read from the wrong row (the visible height in place of the coded
// height) misses the top band.
void expect_bands(const Nv12& p, const std::string& what) {
    const uint32_t w = p.w, h = p.h;
    constexpr int kTol = 12;
    auto expect_near = [&](int got, int want, const char* name, uint32_t x, uint32_t y) {
        // The right half's luma is 120 to 184: 152 with a tolerance of 32.
        const int tol = std::string(name) == "luma" && x > w / 2 ? 32 + kTol : kTol;
        EXPECT_LE(std::abs(got - want), tol) << what << ": " << name << " at " << x << "," << y << " is " << got
                                              << ", not " << want;
    };
    expect_near(p.luma(w / 4, h * 3 / 4), 60, "luma", w / 4, h * 3 / 4);
    expect_near(p.luma(w * 3 / 4, h * 3 / 4), 152, "luma", w * 3 / 4, h * 3 / 4);
    expect_near(p.u(w / 2, 12), 148, "U", w / 2, 12);
    expect_near(p.v(w / 2, 12), 108, "V", w / 2, 12);
    expect_near(p.u(w / 2, h / 4 + 16), 108, "U", w / 2, h / 4 + 16);
    expect_near(p.v(w / 2, h / 4 + 16), 148, "V", w / 2, h / 4 + 16);
    expect_near(p.u(w / 2, h * 3 / 4), 148, "U", w / 2, h * 3 / 4);
    expect_near(p.v(w / 2, h * 3 / 4), 148, "V", w / 2, h * 3 / 4);
}

// ---- the order -----------------------------------------------------------

TEST(WindowsDecoderOrder, NvdecAndAmfComeBeforeMediaFoundation) {
    EXPECT_EQ(windows_decoder_order(false, ""),
              (std::vector<std::string>{"NVDEC", "AMF-Decode", "MediaFoundation", "OpenH264"}));
}

TEST(WindowsDecoderOrder, TheOverrideGoesFirst) {
    EXPECT_EQ(windows_decoder_order(false, "mf"),
              (std::vector<std::string>{"MediaFoundation", "NVDEC", "AMF-Decode", "OpenH264"}));
    EXPECT_EQ(windows_decoder_order(false, "amf"),
              (std::vector<std::string>{"AMF-Decode", "NVDEC", "MediaFoundation", "OpenH264"}));
    EXPECT_EQ(windows_decoder_order(false, "openh264"),
              (std::vector<std::string>{"OpenH264", "NVDEC", "AMF-Decode", "MediaFoundation"}));
    EXPECT_EQ(windows_decoder_order(false, "nvdec"), windows_decoder_order(false, ""));
}

TEST(WindowsDecoderOrder, SoftIsOpenH264OnlyAndAnUnknownOverrideIsIgnored) {
    EXPECT_EQ(windows_decoder_order(true, ""), (std::vector<std::string>{"OpenH264"}));
    EXPECT_EQ(windows_decoder_order(true, "mf"), (std::vector<std::string>{"OpenH264"}));
    EXPECT_EQ(windows_decoder_order(false, "d3d11va"), windows_decoder_order(false, ""));
}

// ---- the device ----------------------------------------------------------

// The Media Foundation decoder decodes on the GPU only on a device made with
// video support. When the driver takes the flag, libmello's device has it.
TEST(WindowsVideoDevice, TheDeviceHasVideoSupportWhenTheDriverTakesIt) {
    ComPtr<ID3D11Device> probe;
    if (FAILED(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
                                 D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, nullptr, 0,
                                 D3D11_SDK_VERSION, &probe, nullptr, nullptr))) {
        GTEST_SKIP() << "the driver refuses video support";
    }
    GraphicsDevice device = create_d3d11_device();
    ASSERT_TRUE(device.handle);
    EXPECT_TRUE(device.d3d11()->GetCreationFlags() & D3D11_CREATE_DEVICE_VIDEO_SUPPORT);
    device.d3d11()->Release();
}

// ---- the decoder ---------------------------------------------------------

class MfDecoderTest : public ::testing::TestWithParam<bool> {
protected:
    void SetUp() override {
        device_ = create_d3d11_device();
        if (!device_.handle) GTEST_SKIP() << "no D3D11 device";
        if (!MfDecoder::is_available()) GTEST_SKIP() << "no Media Foundation H.264 decoder (N edition?)";
        cpu_ = std::make_unique<EnvVar>("MELLO_MF_DECODER_CPU", GetParam() ? "1" : nullptr);
    }
    void TearDown() override {
        cpu_.reset();
        if (device_.handle) device_.d3d11()->Release();
    }
    bool want_gpu() const {
        return !GetParam() && (device_.d3d11()->GetCreationFlags() & D3D11_CREATE_DEVICE_VIDEO_SUPPORT);
    }
    // Decodes each access unit into slot i % 3 and returns the picture after
    // each one (empty texture when it gave none).
    std::vector<DecodedPicture> decode_all(MfDecoder& dec, const Units& u) {
        std::vector<DecodedPicture> pictures;
        for (size_t i = 0; i < u.aus.size(); ++i) {
            const int slot = static_cast<int>(i % Decoder::kOutputSlots);
            DecodedPicture pic;
            const DecodeFeedResult r = dec.decode(u.aus[i].data(), u.aus[i].size(), u.keys[i], slot);
            EXPECT_NE(r, DecodeFeedResult::Error) << "access unit " << i;
            if (r == DecodeFeedResult::FrameReady) EXPECT_TRUE(dec.picture(slot, pic)) << "access unit " << i;
            pictures.push_back(pic);
        }
        return pictures;
    }

    GraphicsDevice device_{};
    std::unique_ptr<EnvVar> cpu_;
};

std::string mode_name(const ::testing::TestParamInfo<bool>& info) {
    return info.param ? "Cpu" : "Gpu";
}

// Low latency: the picture of an access unit comes with that access unit,
// in the mode the device allows.
TEST_P(MfDecoderTest, EachAccessUnitGivesItsPictureInTheSlot) {
    Units u = read_units(kStepFixture);
    ASSERT_EQ(u.aus.size(), 2 * kUnitsPerSize);
    u.aus.resize(kUnitsPerSize);
    u.keys.resize(kUnitsPerSize);
    ASSERT_TRUE(u.keys[0]);

    MfDecoder dec;
    ASSERT_TRUE(dec.initialize(device_, DecoderConfig{640, 360}));
    EXPECT_EQ(dec.gpu_mode(), want_gpu());
    const std::vector<DecodedPicture> pictures = decode_all(dec, u);
    for (size_t i = 0; i < pictures.size(); ++i) {
        ASSERT_TRUE(pictures[i].texture) << "access unit " << i << " gave no picture";
        EXPECT_EQ(pictures[i].format, DXGI_FORMAT_NV12);
        EXPECT_EQ(pictures[i].width, 640u);
        EXPECT_EQ(pictures[i].height, 360u) << "the visible height, not the coded 368";
    }
    // Each picture is the one of its access unit, not a picture of an earlier
    // one again: the right half's luma changes in every frame.
    int last_level = -1;
    for (size_t i = 0; i < pictures.size(); ++i) {
        Nv12 p;
        ASSERT_TRUE(read_nv12(device_.d3d11(), pictures[i].texture.Get(), p)) << "access unit " << i;
        expect_bands(p, std::string(GetParam() ? "cpu" : "gpu") + " access unit " + std::to_string(i));
        const int level = p.luma(p.w * 3 / 4, p.h / 2);
        if (last_level >= 0) {
            EXPECT_GE(std::abs(level - last_level), 8)
                << "access unit " << i << " shows the picture of the access unit before it (luma " << level << ")";
        }
        last_level = level;
    }
    dec.shutdown();
}

TEST_P(MfDecoderTest, A1080pStreamShows1080Rows) {
    const Units u = read_units(k1080pFixture);
    ASSERT_EQ(u.aus.size(), kUnitsPerSize);
    MfDecoder dec;
    ASSERT_TRUE(dec.initialize(device_, DecoderConfig{1920, 1080}));
    const std::vector<DecodedPicture> pictures = decode_all(dec, u);
    ASSERT_TRUE(pictures.back().texture);
    EXPECT_EQ(pictures.back().width, 1920u);
    EXPECT_EQ(pictures.back().height, 1080u) << "1080 visible rows of the 1088 coded rows";
    Nv12 p;
    ASSERT_TRUE(read_nv12(device_.d3d11(), pictures.back().texture.Get(), p));
    EXPECT_EQ(p.h, 1080u);
    expect_bands(p, GetParam() ? "cpu 1080p" : "gpu 1080p");
    dec.shutdown();
}

// A new SPS at a new size (a resolution rung) gives new slot textures at the
// new size; a picture the ring still holds keeps its old texture and size.
TEST_P(MfDecoderTest, ASizeChangeMakesNewSlotTextures) {
    const Units u = read_units(kStepFixture);
    ASSERT_EQ(u.aus.size(), 2 * kUnitsPerSize);
    ASSERT_TRUE(u.keys[kUnitsPerSize]) << "the second size starts with an IDR";
    MfDecoder dec;
    ASSERT_TRUE(dec.initialize(device_, DecoderConfig{640, 360}));
    const std::vector<DecodedPicture> pictures = decode_all(dec, u);
    const DecodedPicture& before = pictures[kUnitsPerSize - 1];
    const DecodedPicture& after = pictures.back();
    ASSERT_TRUE(before.texture);
    ASSERT_TRUE(after.texture);
    EXPECT_EQ(after.width, 320u);
    EXPECT_EQ(after.height, 180u);
    EXPECT_NE(before.texture.Get(), after.texture.Get());
    Nv12 old_pic, new_pic;
    ASSERT_TRUE(read_nv12(device_.d3d11(), before.texture.Get(), old_pic));
    ASSERT_TRUE(read_nv12(device_.d3d11(), after.texture.Get(), new_pic));
    EXPECT_EQ(old_pic.w, 640u) << "the held picture keeps its texture";
    EXPECT_EQ(new_pic.w, 320u);
    expect_bands(new_pic, "after the size change");
    for (size_t i = kUnitsPerSize; i < pictures.size(); ++i) {
        ASSERT_TRUE(pictures[i].texture) << "access unit " << i;
        EXPECT_EQ(pictures[i].height, 180u) << "access unit " << i;
    }
    dec.shutdown();
}

INSTANTIATE_TEST_SUITE_P(Modes, MfDecoderTest, ::testing::Values(false, true), mode_name);

// ---- the factory ---------------------------------------------------------

class WindowsDecoderFactory : public ::testing::Test {
protected:
    void SetUp() override {
        device_ = create_d3d11_device();
        if (!device_.handle) GTEST_SKIP() << "no D3D11 device";
        if (!MfDecoder::is_available()) GTEST_SKIP() << "no Media Foundation H.264 decoder";
    }
    void TearDown() override {
        if (device_.handle) device_.d3d11()->Release();
    }
    GraphicsDevice device_{};
};

TEST_F(WindowsDecoderFactory, TheMfOverrideGivesMediaFoundation) {
    EnvVar forced("MELLO_WINDOWS_DECODER", "mf");
    auto dec = create_best_decoder(device_, DecoderConfig{640, 360});
    ASSERT_TRUE(dec);
    EXPECT_STREQ(dec->name(), "MediaFoundation");
    dec->shutdown();
}

// A machine without NVDEC and AMF (an Intel laptop) gets Media Foundation,
// before OpenH264 (present here when a developer put the DLL in place).
TEST_F(WindowsDecoderFactory, MediaFoundationComesAfterNvdecAndAmf) {
    EnvVar forced("MELLO_WINDOWS_DECODER", nullptr);
    auto dec = create_next_decoder(device_, DecoderConfig{640, 360}, {"NVDEC", "AMF-Decode"});
    ASSERT_TRUE(dec);
    EXPECT_STREQ(dec->name(), "MediaFoundation");
    dec->shutdown();
}

TEST_F(WindowsDecoderFactory, TheDecoderListNamesMediaFoundation) {
    const std::vector<const char*> names = enumerate_decoders(device_);
    bool found = false;
    for (const char* n : names) found = found || std::string(n) == "MediaFoundation";
    EXPECT_TRUE(found);
}

// ---- the viewer's switch -------------------------------------------------

class BrokenDecoder : public Decoder {
public:
    bool initialize(const GraphicsDevice&, const DecoderConfig&) override { return true; }
    void shutdown() override {}
    DecodeFeedResult decode(const uint8_t*, size_t, bool, int) override { return DecodeFeedResult::Error; }
    bool picture(int, DecodedPicture&) const override { return false; }
    bool supports_codec(VideoCodec) const override { return true; }
    const char* name() const override { return "Broken"; }
};

ViewerStats wait_for(ViewerPipeline& viewer, uint64_t errors, const char* decoder = nullptr) {
    ViewerStats s;
    for (int i = 0; i < 300; ++i) {
        viewer.stats(s);
        if (s.decode_errors >= errors && (!decoder || s.decoder_name == decoder)) break;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    return s;
}

class WindowsViewerDecoderSwitch : public WindowsDecoderFactory {};

TEST_F(WindowsViewerDecoderSwitch, ErrorsBeforeTheFirstKeyFrameKeepTheDecoder) {
    EnvVar forced("MELLO_WINDOWS_DECODER", "mf");
    ViewerPipeline viewer;
    ASSERT_TRUE(viewer.start(device_, ViewerConfig{64, 64, 60}, nullptr, std::make_unique<BrokenDecoder>()));
    const uint8_t au[8] = {0, 0, 0, 1, 0x41, 0, 0, 0};
    for (int i = 0; i < 20; ++i) viewer.feed_packet(au, sizeof(au), false);
    const ViewerStats s = wait_for(viewer, 20);
    EXPECT_EQ(s.decode_errors, 20u);
    EXPECT_EQ(s.decoder_name, "Broken");
    viewer.stop();
}

TEST_F(WindowsViewerDecoderSwitch, TwoFailedKeyFramesMoveToTheNextDecoder) {
    EnvVar forced("MELLO_WINDOWS_DECODER", "mf");
    ViewerPipeline viewer;
    ASSERT_TRUE(viewer.start(device_, ViewerConfig{64, 64, 60}, nullptr, std::make_unique<BrokenDecoder>()));
    const uint8_t idr[8] = {0, 0, 0, 1, 0x65, 0, 0, 0};
    viewer.feed_packet(idr, sizeof(idr), true);
    ViewerStats s = wait_for(viewer, 1);
    EXPECT_EQ(s.decoder_name, "Broken");
    viewer.feed_packet(idr, sizeof(idr), true);
    s = wait_for(viewer, 2, "MediaFoundation");
    EXPECT_EQ(s.decoder_name, "MediaFoundation");
    viewer.stop();
}

// ---- the fixtures --------------------------------------------------------

// Encodes the fixture picture at w x h with libmello's host pipeline and the
// OpenH264 encoder (MELLO_SOFT_ENCODER=1) and appends the first
// kUnitsPerSize access units, each after an AUD, to `out`.
bool encode_bands(uint32_t w, uint32_t h, std::vector<uint8_t>& out) {
    const uint32_t frames = 2 * kUnitsPerSize;
    const fs::path path = fs::temp_directory_path() / "mello_mf_fixture.yuv";
    {
        std::vector<uint8_t> frame(reference_frame_bytes(w, h));
        uint8_t* yp = frame.data();
        uint8_t* up = yp + static_cast<size_t>(w) * h;
        uint8_t* vp = up + static_cast<size_t>(w / 2) * (h / 2);
        for (uint32_t y = 0; y < h; ++y)
            for (uint32_t x = 0; x < w; ++x) yp[static_cast<size_t>(y) * w + x] = x < w / 2 ? 60 : 120;
        for (uint32_t y = 0; y < h / 2; ++y) {
            const uint32_t row = y * 2;
            const uint8_t u = row < 32 ? 148 : (row < h / 2 ? 108 : 148);
            const uint8_t v = row < 32 ? 108 : 148;
            std::memset(up + static_cast<size_t>(y) * (w / 2), u, w / 2);
            std::memset(vp + static_cast<size_t>(y) * (w / 2), v, w / 2);
        }
        FILE* f = std::fopen(path.string().c_str(), "wb");
        if (!f) return false;
        for (uint32_t n = 0; n < frames; ++n) {
            const uint8_t right = static_cast<uint8_t>(120 + (n % 5) * 16);
            for (uint32_t y = 0; y < h; ++y) std::memset(yp + static_cast<size_t>(y) * w + w / 2, right, w / 2);
            std::fwrite(frame.data(), 1, frame.size(), f);
        }
        std::fclose(f);
    }
    EnvVar soft("MELLO_SOFT_ENCODER", "1");
    VideoPipeline host;
    if (!host.init_device()) return false;
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Reference;
    desc.reference.path = path.string();
    desc.reference.width = w;
    desc.reference.height = h;
    desc.reference.fps = 60;
    PipelineConfig cfg{};
    cfg.fps = 60;
    cfg.bitrate_kbps = 2000;
    std::mutex mu;
    std::vector<std::vector<uint8_t>> aus;
    if (!host.start_host(desc, cfg, [&](const uint8_t* d, size_t n, bool, uint64_t) {
            std::lock_guard<std::mutex> lock(mu);
            aus.emplace_back(d, d + n);
        })) {
        return false;
    }
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
    while (std::chrono::steady_clock::now() < deadline) {
        {
            std::lock_guard<std::mutex> lock(mu);
            if (aus.size() >= kUnitsPerSize) break;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    host.stop_host();
    fs::remove(path);
    if (aus.size() < kUnitsPerSize || !replay::h264::has_idr(aus[0].data(), aus[0].size())) return false;
    static const uint8_t kAud[] = {0, 0, 0, 1, 0x09, 0xF0};
    for (size_t i = 0; i < kUnitsPerSize; ++i) {
        out.insert(out.end(), kAud, kAud + sizeof(kAud));
        out.insert(out.end(), aus[i].begin(), aus[i].end());
    }
    return true;
}

void write_file(const char* name, const std::vector<uint8_t>& bytes) {
    std::ofstream f(fs::path(MELLO_VIDEO_FIXTURE_DIR) / name, std::ios::binary);
    f.write(reinterpret_cast<const char*>(bytes.data()), static_cast<std::streamsize>(bytes.size()));
}

// Makes the fixtures again (fixtures/video/README.md). Not part of the
// suite: it needs the OpenH264 DLL. Run:
//   mello_tests --gtest_also_run_disabled_tests --gtest_filter=MfFixtures.*
TEST(MfFixtures, DISABLED_Write) {
    std::vector<uint8_t> step, full;
    ASSERT_TRUE(encode_bands(640, 360, step));
    ASSERT_TRUE(encode_bands(320, 180, step));
    ASSERT_TRUE(encode_bands(1920, 1080, full));
    write_file(kStepFixture, step);
    write_file(k1080pFixture, full);
}

} // namespace
} // namespace mello::video
#endif
