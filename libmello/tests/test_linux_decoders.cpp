// Linux decoder choice (linux_gpu.cpp) and the viewer's switch to the next
// decoder when one stops working (video_viewer.cpp).
#if defined(__linux__)
#include "video/linux_gpu.hpp"
#include "video/encoder_gst.hpp"
#include "video/decoder_openh264.hpp"
#include "video/graphics_device.hpp"
#include "video/video_viewer.hpp"
#include <gtest/gtest.h>

#include <atomic>
#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <string>
#include <sys/stat.h>
#include <thread>
#include <unistd.h>

namespace mello::video {
namespace {

// A fake /sys/class/drm: card<N>/device -> ../devices/<pci>, renderD<M>/device
// -> the same PCI directory.
class FakeDrm {
public:
    FakeDrm() {
        char tmpl[] = "/tmp/m3llo-drm-XXXXXX";
        root_ = mkdtemp(tmpl);
        mkdir((root_ + "/devices").c_str(), 0755);
        mkdir((root_ + "/drm").c_str(), 0755);
    }
    ~FakeDrm() {
        std::error_code ec;
        std::filesystem::remove_all(root_, ec);
    }

    void add(const std::string& card, const std::string& render, const char* pci, const char* vendor,
             const char* boot_vga) {
        const std::string dev = root_ + "/devices/" + pci;
        mkdir(dev.c_str(), 0755);
        std::ofstream(dev + "/vendor") << vendor << "\n";
        if (boot_vga) std::ofstream(dev + "/boot_vga") << boot_vga << "\n";
        for (const std::string& node : {card, render}) {
            if (node.empty()) continue;
            mkdir((root_ + "/drm/" + node).c_str(), 0755);
            EXPECT_EQ(symlink(dev.c_str(), (root_ + "/drm/" + node + "/device").c_str()), 0);
        }
    }
    std::string drm() const { return root_ + "/drm"; }

private:
    std::string root_;
};

TEST(LinuxGpu, AHybridLaptopFindsItsBootGpuAndRenderNodes) {
    FakeDrm fs;
    fs.add("card0", "renderD129", "0000:01:00.0", "0x10de", "0");
    fs.add("card1", "renderD128", "0000:05:00.0", "0x1002", "1");
    // A connector entry is not a GPU.
    mkdir((fs.drm() + "/card1-eDP-1").c_str(), 0755);
    const auto gpus = list_linux_gpus(fs.drm());
    ASSERT_EQ(gpus.size(), 2u);
    EXPECT_EQ(gpus[0].vendor, kVendorNvidia);
    EXPECT_FALSE(gpus[0].primary);
    EXPECT_EQ(gpus[0].render_node, "/dev/dri/renderD129");
    EXPECT_EQ(gpus[1].vendor, kVendorAmd);
    EXPECT_TRUE(gpus[1].primary);
    EXPECT_EQ(primary_render_node(gpus), "/dev/dri/renderD128");
}

TEST(LinuxGpu, NoDrmTreeIsNoGpu) {
    EXPECT_TRUE(list_linux_gpus("/nonexistent/drm").empty());
}




constexpr uint64_t GiB = 1ull << 30;

// As on Windows, one GPU does all video work: the one with the most
// dedicated memory, whatever the brand.
TEST(LinuxVideoGpu, TheGpuWithTheMostVideoMemoryWinsWhateverTheBrand) {
    // A hybrid laptop: the AMD APU shows its carve-out, NVIDIA nothing.
    std::vector<LinuxGpu> hybrid = {{kVendorNvidia, false, "/dev/dri/renderD129"},
                                    {kVendorAmd, true, "/dev/dri/renderD128", "", GiB / 2}};
    EXPECT_EQ(video_gpu(hybrid)->vendor, kVendorNvidia);
    EXPECT_EQ(video_render_node(hybrid), "/dev/dri/renderD129");
    // A discrete AMD card beats an NVIDIA GPU of unknown memory...
    std::vector<LinuxGpu> desk = {{kVendorNvidia, false, "/dev/dri/renderD129"},
                                  {kVendorAmd, true, "/dev/dri/renderD128", "", 8 * GiB}};
    EXPECT_EQ(video_gpu(desk)->vendor, kVendorAmd);
    // ...and loses to one that reports more (NVML).
    desk[0].vram_bytes = 12 * GiB;
    EXPECT_EQ(video_gpu(desk)->vendor, kVendorNvidia);
    // Two AMD GPUs: the discrete one.
    std::vector<LinuxGpu> amd = {{kVendorAmd, true, "/dev/dri/renderD128", "", GiB / 2},
                                 {kVendorAmd, false, "/dev/dri/renderD129", "", 8 * GiB}};
    EXPECT_EQ(video_render_node(amd), "/dev/dri/renderD129");
    // A tie goes to the primary GPU.
    std::vector<LinuxGpu> intel = {{kVendorIntel, false, "/dev/dri/renderD129"},
                                   {kVendorIntel, true, "/dev/dri/renderD128"}};
    EXPECT_EQ(video_render_node(intel), "/dev/dri/renderD128");
    EXPECT_EQ(video_gpu({}), nullptr);
}

TEST(LinuxDecoderOrder, AHybridLaptopDecodesOnItsNvidiaGpuAsWindowsDoes) {
    std::vector<LinuxGpu> gpus = {{kVendorNvidia, false, "/dev/dri/renderD129"},
                                  {kVendorAmd, true, "/dev/dri/renderD128", "", GiB / 2}};
    EXPECT_EQ(linux_decoder_order(gpus, false, ""),
              (std::vector<std::string>{"NVDEC", "VA-API", "OpenH264"}));
    EXPECT_EQ(linux_encoder_order(gpus, false, ""), (std::vector<std::string>{"NVENC", "VA-API"}));
}

TEST(LinuxVaDriver, TheVideoGpusVaDriverIsNamedWithTwoGpus) {
    char tmpl[] = "/tmp/m3llo-va-XXXXXX";
    const std::string dir = mkdtemp(tmpl);
    std::ofstream(dir + "/radeonsi_drv_video.so") << "";
    // An AMD card with an Intel iGPU: radeonsi.
    std::vector<LinuxGpu> gpus = {{kVendorIntel, true, "/dev/dri/renderD128"},
                                  {kVendorAmd, false, "/dev/dri/renderD129", "", 8 * GiB}};
    EXPECT_EQ(va_driver_for_video_gpu(gpus, {"/nonexistent", dir}), "radeonsi");
    // One GPU: libva picks the driver itself.
    EXPECT_EQ(va_driver_for_video_gpu({gpus[1]}, {dir}), "");
    // NVIDIA as the video GPU decodes with NVDEC: VA is left alone.
    std::vector<LinuxGpu> hybrid = {{kVendorNvidia, false, "/dev/dri/renderD129"},
                                    {kVendorAmd, true, "/dev/dri/renderD128", "", GiB / 2}};
    EXPECT_EQ(va_driver_for_video_gpu(hybrid, {dir}), "");
    // Two Intel GPUs: the driver that is installed.
    std::vector<LinuxGpu> intel = {{kVendorIntel, true, "/dev/dri/renderD128"},
                                   {kVendorIntel, false, "/dev/dri/renderD129"}};
    EXPECT_EQ(va_driver_for_video_gpu(intel, {dir}), "");
    std::ofstream(dir + "/iHD_drv_video.so") << "";
    EXPECT_EQ(va_driver_for_video_gpu(intel, {dir}), "iHD");
    std::error_code ec;
    std::filesystem::remove_all(dir, ec);
}

TEST(LinuxDecoderOrder, AnNvidiaDesktopTakesNvdecFirst) {
    std::vector<LinuxGpu> gpus = {{kVendorNvidia, true, "/dev/dri/renderD128"}};
    EXPECT_EQ(linux_decoder_order(gpus, false, ""),
              (std::vector<std::string>{"NVDEC", "VA-API", "OpenH264"}));
    // One GPU without boot_vga is the primary one.
    gpus[0].primary = false;
    EXPECT_EQ(linux_decoder_order(gpus, false, "")[0], "NVDEC");
}

TEST(LinuxDecoderOrder, TheOverrideGoesFirstAndSoftIsOpenH264Only) {
    std::vector<LinuxGpu> gpus = {{kVendorAmd, true, "/dev/dri/renderD128"}};
    EXPECT_EQ(linux_decoder_order(gpus, false, "nvdec"),
              (std::vector<std::string>{"NVDEC", "VA-API", "OpenH264"}));
    EXPECT_EQ(linux_decoder_order(gpus, false, "openh264"),
              (std::vector<std::string>{"OpenH264", "VA-API", "NVDEC"}));
    EXPECT_EQ(linux_decoder_order(gpus, false, "bogus"),
              (std::vector<std::string>{"VA-API", "NVDEC", "OpenH264"}));
    EXPECT_EQ(linux_decoder_order(gpus, true, "nvdec"), (std::vector<std::string>{"OpenH264"}));
}

// As on Windows, streams encode on the discrete GPU: on a hybrid laptop
// that is the NVIDIA GPU the game runs on, not the boot GPU.
TEST(LinuxEncoderOrder, TheDiscreteGpusEncoderFirstAndNoSoftwareEncoder) {
    std::vector<LinuxGpu> hybrid = {{kVendorNvidia, false, "/dev/dri/renderD129"},
                                    {kVendorAmd, true, "/dev/dri/renderD128", "", 512ull << 20}};
    EXPECT_EQ(linux_encoder_order(hybrid, false, ""), (std::vector<std::string>{"NVENC", "VA-API"}));
    EXPECT_EQ(video_render_node(hybrid), "/dev/dri/renderD129");
    std::vector<LinuxGpu> amd_only = {{kVendorAmd, true, "/dev/dri/renderD128"}};
    EXPECT_EQ(linux_encoder_order(amd_only, false, ""), (std::vector<std::string>{"VA-API", "NVENC"}));
}

TEST(LinuxEncoderOrder, TwoAmdGpusEncodeOnTheOneWithMoreVideoMemory) {
    std::vector<LinuxGpu> gpus = {{kVendorAmd, true, "/dev/dri/renderD128", "", 512ull << 20},
                                  {kVendorAmd, false, "/dev/dri/renderD129", "", 8ull << 30}};
    EXPECT_EQ(video_render_node(gpus), "/dev/dri/renderD129");
    EXPECT_EQ(linux_encoder_order(gpus, false, "")[0], "VA-API");
}

TEST(LinuxEncoderOrder, SoftwareOnlyOnRequestAndTheOverrideGoesFirst) {
    std::vector<LinuxGpu> amd = {{kVendorAmd, true, "/dev/dri/renderD128"}};
    EXPECT_EQ(linux_encoder_order(amd, true, "va"), (std::vector<std::string>{"OpenH264"}));
    EXPECT_EQ(linux_encoder_order(amd, false, "openh264"), (std::vector<std::string>{"OpenH264"}));
    EXPECT_EQ(linux_encoder_order(amd, false, "nvenc"), (std::vector<std::string>{"NVENC", "VA-API"}));
    std::vector<LinuxGpu> nvidia = {{kVendorNvidia, true, "/dev/dri/renderD128"}};
    EXPECT_EQ(linux_encoder_order(nvidia, false, "va"), (std::vector<std::string>{"VA-API", "NVENC"}));
}

// vapostproc's sink caps list DMA-BUF layouts as drm-format strings. Only
// the 32-bit RGB ones matter to the capture; a name alone is linear.
TEST(VaDmaBufFormats, TheRgbLayoutsComeOutOfTheCaps) {
    const std::string caps =
        "video/x-raw(memory:DMABuf), width=(int)[ 1, 16384 ], height=(int)[ 1, 16384 ], "
        "format=(string)DMA_DRM, drm-format=(string){ NV12:0x0200000000000901, "
        "XR24:0x0200000000000901, AR24:0x0200000000000901, XR24, YUYV, XB24:0x0200000000000a01 }";
    const auto formats = parse_rgb_drm_formats(caps);
    ASSERT_EQ(formats.size(), 3u);
    EXPECT_EQ(formats[0].drm_fourcc, kDrmXRGB8888);
    EXPECT_EQ(formats[0].modifiers, (std::vector<uint64_t>{0x0200000000000901ull, 0}));
    EXPECT_EQ(formats[1].drm_fourcc, kDrmARGB8888);
    EXPECT_EQ(formats[2].drm_fourcc, kDrmXBGR8888);
    EXPECT_EQ(formats[2].modifiers, (std::vector<uint64_t>{0x0200000000000a01ull}));
}

TEST(VaDmaBufFormats, CapsWithoutRgbGiveNothing) {
    EXPECT_TRUE(parse_rgb_drm_formats("video/x-raw(memory:DMABuf), drm-format=(string)NV12").empty());
    EXPECT_TRUE(parse_rgb_drm_formats("").empty());
}

// A decoder that accepts nothing.
class BrokenDecoder : public Decoder {
public:
    bool initialize(const GraphicsDevice&, const DecoderConfig&) override { return true; }
    void shutdown() override { shut_down = true; }
    DecodeFeedResult decode(const uint8_t*, size_t, bool, int) override { return DecodeFeedResult::Error; }
    bool picture(int, CpuPicture&) const override { return false; }
    bool supports_codec(VideoCodec) const override { return true; }
    const char* name() const override { return "Broken"; }
    std::atomic<bool> shut_down{false};
};

ViewerStats wait_for_errors(ViewerPipeline& viewer, uint64_t errors) {
    ViewerStats s;
    for (int i = 0; i < 300; ++i) {
        viewer.stats(s);
        if (s.decode_errors >= errors) break;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    return s;
}

class ViewerDecoderSwitch : public ::testing::Test {
protected:
    void SetUp() override {
        // OpenH264 is the only next decoder; without it nothing can switch.
        setenv("MELLO_SOFT_DECODER", "1", 1);
        if (!OpenH264Decoder::is_available()) GTEST_SKIP() << "no OpenH264 (soname 8)";
    }
    void TearDown() override { unsetenv("MELLO_SOFT_DECODER"); }
};

TEST_F(ViewerDecoderSwitch, ErrorsBeforeTheFirstKeyFrameKeepTheDecoder) {
    ViewerPipeline viewer;
    ASSERT_TRUE(viewer.start(create_cpu_device(), ViewerConfig{64, 64, 60}, nullptr,
                             std::make_unique<BrokenDecoder>()));
    const uint8_t au[8] = {0, 0, 0, 1, 0x41, 0, 0, 0};
    for (int i = 0; i < 20; ++i) viewer.feed_packet(au, sizeof(au), false);
    const ViewerStats s = wait_for_errors(viewer, 20);
    EXPECT_EQ(s.decode_errors, 20u);
    EXPECT_EQ(s.decoder_name, "Broken");
    viewer.stop();
}

TEST_F(ViewerDecoderSwitch, TwoFailedKeyFramesMoveToTheNextDecoder) {
    ViewerPipeline viewer;
    ASSERT_TRUE(viewer.start(create_cpu_device(), ViewerConfig{64, 64, 60}, nullptr,
                             std::make_unique<BrokenDecoder>()));
    const uint8_t idr[8] = {0, 0, 0, 1, 0x65, 0, 0, 0};
    viewer.feed_packet(idr, sizeof(idr), true);
    ViewerStats s = wait_for_errors(viewer, 1);
    EXPECT_EQ(s.decoder_name, "Broken");
    viewer.feed_packet(idr, sizeof(idr), true);
    s = wait_for_errors(viewer, 2);
    for (int i = 0; i < 100 && s.decoder_name != "OpenH264"; ++i) {
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
        viewer.stats(s);
    }
    EXPECT_EQ(s.decoder_name, "OpenH264");
    viewer.stop();
}

} // namespace
} // namespace mello::video
#endif
