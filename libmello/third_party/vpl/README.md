# Intel oneVPL (Video Processing Library) Headers

Required for QSV (Quick Sync Video) encode support on Intel GPUs.

## Setup

1. Clone or download from: https://github.com/intel/libvpl

2. Copy the `api/vpl/` headers into this directory:

```
vpl/
├── include/
│   └── vpl/
│       ├── mfx.h
│       ├── mfxdefs.h
│       ├── mfxstructures.h
│       ├── mfxvideo.h
│       ├── mfxdispatcher.h
│       └── ...
```

3. Re-run CMake — it will detect the headers and define `MELLO_HAS_QSV`.

## Runtime

The oneVPL dispatcher (`libvpl.dll` or `libmfx64.dll`) is loaded at runtime.
Ships with Intel graphics drivers. If not present, QSV is skipped.
