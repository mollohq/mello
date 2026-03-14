# AMD Advanced Media Framework (AMF) SDK Headers

Required for AMF encode and decode support on AMD GPUs.

## Setup

1. Clone or download from: https://github.com/GPUOpen-LibrariesAndSDKs/AMF

2. Copy the `amf/public/include/` tree into this directory:

```
amf/
├── include/
│   └── AMF/
│       ├── core/
│       │   ├── Factory.h
│       │   ├── Context.h
│       │   ├── Surface.h
│       │   ├── Buffer.h
│       │   └── ...
│       └── components/
│           ├── VideoEncoderVCE.h    # H.264 encoder
│           ├── VideoEncoderAV1.h    # AV1 encoder
│           ├── VideoDecoderUVD.h    # H.264/AV1 decoder
│           └── ...
```

3. Re-run CMake — it will detect the headers and define `MELLO_HAS_AMF`.

## Runtime

No link-time dependency. `amfrt64.dll` is loaded at runtime — ships with AMD drivers.
If not present, the encoder/decoder factory gracefully skips AMD.
