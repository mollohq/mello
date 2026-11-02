// The default device name out of WirePlumber's "default" metadata. Linux only:
// the PipeWire backend is compiled on Linux and nowhere else.
#if defined(__linux__)
#include <gtest/gtest.h>
#include "audio/pipewire_stream.hpp"

using mello::audio::pw::parse_default_node_name;

TEST(PipeWireMetadata, ReadsTheNodeName) {
    EXPECT_EQ(parse_default_node_name(
                  R"({"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"})"),
              "alsa_output.pci-0000_00_1f.3.analog-stereo");
}

TEST(PipeWireMetadata, AllowsSpacesAndOtherKeys) {
    EXPECT_EQ(parse_default_node_name(R"({ "priority": 1, "name" : "bluez_input.headset" })"),
              "bluez_input.headset");
}

TEST(PipeWireMetadata, KeepsEscapedQuotes) {
    EXPECT_EQ(parse_default_node_name(R"({"name":"odd \"name\""})"), "odd \"name\"");
}

TEST(PipeWireMetadata, EmptyWithoutAName) {
    EXPECT_EQ(parse_default_node_name(nullptr), "");
    EXPECT_EQ(parse_default_node_name(""), "");
    EXPECT_EQ(parse_default_node_name(R"({"other":"x"})"), "");
    EXPECT_EQ(parse_default_node_name(R"({"name":"unterminated)"), "");
}
#endif
