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

The runtime loader tries these names in order:
- `openh264-2.6.0-win64.dll`
- `openh264-2.5.0-win64.dll`
- `openh264.dll`

## Licensing

OpenH264 is BSD-2-Clause licensed. Cisco pays the MPEG-LA royalties for
their prebuilt binaries. If you build from source, YOU are responsible
for MPEG-LA licensing — so don't.
