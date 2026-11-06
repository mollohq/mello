# OpenH264 — Cisco Prebuilt Binary

**You MUST use the official Cisco prebuilt binary.** Do not build from source.

Cisco's prebuilt binaries are covered by their MPEG-LA patent license grant,
which means you can distribute them without paying H.264 royalties.

## Download

1. Go to: https://github.com/cisco/openh264/releases

2. Download the latest Windows x64 DLL (currently v2.6.0):
   http://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2

3. Decompress the `.bz2` and place the DLL in this directory:

```
libmello/third_party/openh264/openh264-2.6.0-win64.dll
```

The build system will automatically copy it next to the output binary.

**Windows release builds ship no OpenH264.** An installer that carries the
DLL breaks Cisco's binary licence (section Licensing). The DLL here is for
development only: the stream gate's CPU lane (`MELLO_SOFT_ENCODER=1`,
`MELLO_SOFT_DECODER=1`) and the tests that need the OpenH264 encoder. A
Windows viewer without NVDEC or AMF decodes with the Microsoft H.264 decoder
through Media Foundation (`decoder_mf.cpp`, 12-STREAMING.md section 7.4).

The runtime loader tries these names in order:
- `openh264-2.6.0-win64.dll`
- `openh264-2.5.0-win64.dll`
- `openh264.dll`

## Licensing

OpenH264 is BSD-2-Clause licensed. Cisco pays the MPEG-LA royalties for
their prebuilt binaries. If you build from source, YOU are responsible
for MPEG-LA licensing — so don't.

`BINARY_LICENSE.txt` is Cisco's licence for the binary (v1.0, from
https://www.openh264.org/BINARY_LICENSE.txt). The third-party notices show
it in full (`client/build_notices.rs`). Its patent conditions ask that the
binary is downloaded to the user's device by itself, that the user can turn
it off, and that the text "OpenH264 Video Codec provided by Cisco Systems,
Inc." is shown at that switch. On Linux the client downloads the binary at
the first start (mello-core `stream/openh264.rs`), and the Audio & Video
settings have the switch.
