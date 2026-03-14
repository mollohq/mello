# NVIDIA Video Codec SDK Headers

Required for NVENC (encode) and NVDEC (decode) support.

## Setup

1. Download from: https://developer.nvidia.com/video-codec-sdk
   (Requires free NVIDIA developer account)
   
2. Extract and copy these files into this directory:

```
nv-codec/
├── include/
│   ├── nvEncodeAPI.h        # NVENC encoder API
│   ├── nvcuvid.h            # NVDEC parser API
│   ├── cuviddec.h           # NVDEC decoder API
│   └── dynlink_cuda.h       # CUDA type definitions (optional)
```

3. Re-run CMake — it will detect the headers and define `MELLO_HAS_NVENC`.

## Runtime

No link-time dependency. The following DLLs are loaded at runtime:
- `nvEncodeAPI64.dll` (NVENC) — ships with NVIDIA drivers
- `nvcuvid.dll` (NVDEC) — ships with NVIDIA drivers

If these DLLs are not present, the encoder/decoder factory gracefully skips NVIDIA.
