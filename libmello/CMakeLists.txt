cmake_minimum_required(VERSION 3.20)
project(mello VERSION 0.1.0 LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

# Dependencies via vcpkg
find_package(Opus CONFIG REQUIRED)
find_package(LibDataChannel CONFIG REQUIRED)

# dav1d uses meson/pkgconfig — no CMake config. Try pkg-config first,
# fall back to probing the vcpkg installed path directly.
find_package(PkgConfig QUIET)
if(PkgConfig_FOUND)
    pkg_check_modules(DAV1D QUIET IMPORTED_TARGET dav1d)
endif()
if(NOT DAV1D_FOUND)
    # Manual fallback: check vcpkg installed dir
    find_path(DAV1D_INCLUDE_DIR NAMES dav1d/dav1d.h)
    find_library(DAV1D_LIBRARY NAMES dav1d)
    if(DAV1D_INCLUDE_DIR AND DAV1D_LIBRARY)
        set(DAV1D_FOUND TRUE)
        add_library(dav1d::dav1d STATIC IMPORTED)
        set_target_properties(dav1d::dav1d PROPERTIES
            IMPORTED_LOCATION "${DAV1D_LIBRARY}"
            INTERFACE_INCLUDE_DIRECTORIES "${DAV1D_INCLUDE_DIR}"
        )
    endif()
endif()

# ---- Hardware SDK detection (header-only, runtime-loaded) ----
# SDK headers are bundled in the repo (MIT / Apache 2.0 — redistributable).
# All three encoder/decoder backends are always compiled; runtime LoadLibrary
# determines which are actually available on the user's machine.
set(MELLO_NV_CODEC_DIR ${CMAKE_CURRENT_SOURCE_DIR}/third_party/nv-codec/include)
set(MELLO_AMF_DIR      ${CMAKE_CURRENT_SOURCE_DIR}/third_party)
set(MELLO_VPL_DIR      ${CMAKE_CURRENT_SOURCE_DIR}/third_party)

# OpenH264 decoder: Cisco prebuilt DLL loaded at runtime (not linked).
# Always enabled — the DLL is shipped next to the binary.
message(STATUS "[mello] OpenH264 decoder enabled (Cisco prebuilt DLL, runtime-loaded)")

# dav1d AV1 decoder: statically linked via vcpkg
if(DAV1D_FOUND)
    set(MELLO_HAS_DAV1D TRUE)
    message(STATUS "[mello] dav1d found — AV1 software decoder enabled")
else()
    set(MELLO_HAS_DAV1D FALSE)
    message(STATUS "[mello] dav1d not found — AV1 software decoder disabled")
endif()

# ONNX Runtime — prebuilt shared library, auto-downloaded
set(ORT_VERSION "1.23.2")
set(ORT_BASE_DIR ${CMAKE_CURRENT_SOURCE_DIR}/third_party/onnxruntime)

if(WIN32)
    set(ORT_ARCHIVE "onnxruntime-win-x64-${ORT_VERSION}.zip")
    set(ORT_URL "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/${ORT_ARCHIVE}")
    set(ORT_DIR "${ORT_BASE_DIR}/onnxruntime-win-x64-${ORT_VERSION}")
elseif(APPLE)
    set(ORT_ARCHIVE "onnxruntime-osx-arm64-${ORT_VERSION}.tgz")
    set(ORT_URL "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/${ORT_ARCHIVE}")
    set(ORT_DIR "${ORT_BASE_DIR}/onnxruntime-osx-arm64-${ORT_VERSION}")
else()
    set(ORT_ARCHIVE "onnxruntime-linux-x64-${ORT_VERSION}.tgz")
    set(ORT_URL "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/${ORT_ARCHIVE}")
    set(ORT_DIR "${ORT_BASE_DIR}/onnxruntime-linux-x64-${ORT_VERSION}")
endif()

if(NOT EXISTS "${ORT_DIR}/include")
    set(ORT_DL_PATH "${CMAKE_CURRENT_BINARY_DIR}/${ORT_ARCHIVE}")
    if(NOT EXISTS "${ORT_DL_PATH}")
        message(STATUS "[mello] Downloading ONNX Runtime v${ORT_VERSION}...")
        file(DOWNLOAD "${ORT_URL}" "${ORT_DL_PATH}" SHOW_PROGRESS STATUS ORT_DL_STATUS)
        list(GET ORT_DL_STATUS 0 ORT_DL_CODE)
        if(NOT ORT_DL_CODE EQUAL 0)
            list(GET ORT_DL_STATUS 1 ORT_DL_MSG)
            message(FATAL_ERROR "[mello] ONNX Runtime download failed: ${ORT_DL_MSG}")
        endif()
    endif()
    file(ARCHIVE_EXTRACT INPUT "${ORT_DL_PATH}" DESTINATION "${ORT_BASE_DIR}")
    message(STATUS "[mello] ONNX Runtime v${ORT_VERSION} extracted to ${ORT_DIR}")
endif()

if(NOT EXISTS "${ORT_DIR}/include")
    message(FATAL_ERROR "[mello] ONNX Runtime not found at ${ORT_DIR} after download — VAD requires it")
endif()

add_library(onnxruntime SHARED IMPORTED)
if(WIN32)
    set_target_properties(onnxruntime PROPERTIES
        IMPORTED_LOCATION "${ORT_DIR}/lib/onnxruntime.dll"
        IMPORTED_IMPLIB "${ORT_DIR}/lib/onnxruntime.lib"
        INTERFACE_INCLUDE_DIRECTORIES "${ORT_DIR}/include"
    )
elseif(APPLE)
    set_target_properties(onnxruntime PROPERTIES
        IMPORTED_LOCATION "${ORT_DIR}/lib/libonnxruntime.dylib"
        INTERFACE_INCLUDE_DIRECTORIES "${ORT_DIR}/include"
    )
else()
    set_target_properties(onnxruntime PROPERTIES
        IMPORTED_LOCATION "${ORT_DIR}/lib/libonnxruntime.so"
        INTERFACE_INCLUDE_DIRECTORIES "${ORT_DIR}/include"
    )
endif()
message(STATUS "[mello] ONNX Runtime v${ORT_VERSION} found at ${ORT_DIR}")

# Build rnnoise from source (vcpkg port uses autotools, unsupported on Windows)
set(RNNOISE_DIR ${CMAKE_CURRENT_SOURCE_DIR}/third_party/rnnoise)

# Download model data (rnnoise_data.c/.h) if missing — same source as vcpkg portfile
set(RNNOISE_MODEL_TAR "rnnoise_data-0a8755f8e2d834eff6a54714ecc7d75f9932e845df35f8b59bc52a7cfe6e8b37.tar.gz")
if(NOT EXISTS "${RNNOISE_DIR}/src/rnnoise_data.h")
    set(RNNOISE_MODEL_URL "https://media.xiph.org/rnnoise/models/${RNNOISE_MODEL_TAR}")
    set(RNNOISE_MODEL_DL "${CMAKE_CURRENT_BINARY_DIR}/${RNNOISE_MODEL_TAR}")
    if(NOT EXISTS "${RNNOISE_MODEL_DL}")
        message(STATUS "[mello] Downloading rnnoise model data...")
        file(DOWNLOAD "${RNNOISE_MODEL_URL}" "${RNNOISE_MODEL_DL}"
             EXPECTED_HASH SHA512=b327d2fc5095be9ed66c5246a86b1a1ce180e9de875c4e5e8778f975560d1f035da40a8686dc1c3fd91c8e709be65d2638eccaa9f866b6f3d85f8d0d16bd2184
             SHOW_PROGRESS)
    endif()
    file(ARCHIVE_EXTRACT INPUT "${RNNOISE_MODEL_DL}" DESTINATION "${CMAKE_CURRENT_BINARY_DIR}/rnnoise_model")
    file(COPY "${CMAKE_CURRENT_BINARY_DIR}/rnnoise_model/src/rnnoise_data.c" DESTINATION "${RNNOISE_DIR}/src/")
    file(COPY "${CMAKE_CURRENT_BINARY_DIR}/rnnoise_model/src/rnnoise_data.h" DESTINATION "${RNNOISE_DIR}/src/")
    message(STATUS "[mello] rnnoise model data extracted")
endif()

set(RNNOISE_SOURCES
    ${RNNOISE_DIR}/src/denoise.c
    ${RNNOISE_DIR}/src/rnn.c
    ${RNNOISE_DIR}/src/rnnoise_data.c
    ${RNNOISE_DIR}/src/rnnoise_tables.c
    ${RNNOISE_DIR}/src/nnet.c
    ${RNNOISE_DIR}/src/nnet_default.c
    ${RNNOISE_DIR}/src/parse_lpcnet_weights.c
    ${RNNOISE_DIR}/src/pitch.c
    ${RNNOISE_DIR}/src/kiss_fft.c
    ${RNNOISE_DIR}/src/celt_lpc.c
)
add_library(rnnoise STATIC ${RNNOISE_SOURCES})
target_include_directories(rnnoise PUBLIC ${RNNOISE_DIR}/include)
target_include_directories(rnnoise PRIVATE ${RNNOISE_DIR}/src)
target_compile_definitions(rnnoise PRIVATE COMPILE_OPUS _USE_MATH_DEFINES)
if(MSVC)
    target_compile_options(rnnoise PRIVATE /wd4244 /wd4305 /wd4267)
endif()

# Platform-agnostic sources
set(MELLO_SOURCES
    src/mello.cpp
    src/context.cpp
    src/audio/audio_pipeline.cpp
    src/audio/opus_codec.cpp
    src/audio/noise_suppressor.cpp
    src/audio/jitter_buffer.cpp
    src/audio/vad.cpp
    src/transport/peer_connection.cpp
    src/video/cursor.cpp
    src/video/encoder_factory.cpp
    src/video/decoder_factory.cpp
)

# Platform-specific audio sources
if(WIN32)
    list(APPEND MELLO_SOURCES
        src/audio/capture_wasapi.cpp
        src/audio/playback_wasapi.cpp
        src/audio/audio_backend_wasapi.cpp
        src/audio/device_enumerator_wasapi.cpp
        src/audio/mic_permission_stub.cpp
    )
elseif(APPLE)
    list(APPEND MELLO_SOURCES
        src/audio/capture_coreaudio.cpp
        src/audio/playback_coreaudio.cpp
        src/audio/audio_backend_coreaudio.cpp
        src/audio/device_enumerator_coreaudio.cpp
        src/audio/mic_permission_macos.mm
    )
else()
    list(APPEND MELLO_SOURCES
        src/audio/audio_backend_stub.cpp
        src/audio/device_enumerator_stub.cpp
        src/audio/mic_permission_stub.cpp
    )
endif()

# Platform-specific video sources (Windows only for now)
if(WIN32)
    list(APPEND MELLO_SOURCES
        src/video/graphics_device.cpp
        src/video/video_pipeline.cpp
        src/video/capture_dxgi.cpp
        src/video/capture_wgc.cpp
        src/video/capture_process.cpp
        src/video/process_enum.cpp
        src/video/video_preprocessor.cpp
        src/video/staging_texture.cpp
        src/video/encoder_nvenc.cpp
        src/video/encoder_amf.cpp
        src/video/encoder_qsv.cpp
        src/video/decoder_nvdec.cpp
        src/video/decoder_amf.cpp
        src/video/decoder_d3d11va.cpp
        src/video/decoder_openh264.cpp
        src/video/decoder_dav1d.cpp
        src/video/openh264_dynload.cpp
    )
else()
    list(APPEND MELLO_SOURCES
        src/video/graphics_device.cpp
        src/video/video_pipeline.cpp
        src/video/process_enum.cpp
    )
endif()

# Static library for Rust FFI linking
add_library(mello STATIC ${MELLO_SOURCES})

target_compile_definitions(mello PRIVATE MELLO_EXPORTS NOMINMAX)

target_include_directories(mello PRIVATE
    ${MELLO_NV_CODEC_DIR}
    ${MELLO_AMF_DIR}
    ${MELLO_VPL_DIR}
)
if(DEFINED ENV{CUDA_PATH})
    target_include_directories(mello PRIVATE "$ENV{CUDA_PATH}/include")
endif()
if(MELLO_HAS_DAV1D)
    target_compile_definitions(mello PRIVATE MELLO_HAS_DAV1D)
endif()

if(MSVC)
    target_compile_options(mello PRIVATE /EHsc /await)
endif()

target_include_directories(mello
    PUBLIC  include
    PRIVATE src
)

target_link_libraries(mello
    PUBLIC
        Opus::opus
        LibDataChannel::LibDataChannel
        rnnoise
        onnxruntime
)

if(WIN32)
    target_link_libraries(mello PUBLIC
        ole32
        winmm
        ksuser
        mfplat
        mfuuid
        avrt
        d3d11
        dxgi
        dxguid
        d3dcompiler
        windowsapp
    )
    # OpenH264 is NOT linked — Cisco prebuilt DLL loaded at runtime via LoadLibrary
elseif(APPLE)
    target_link_libraries(mello PUBLIC
        "-framework AudioToolbox"
        "-framework CoreAudio"
        "-framework CoreFoundation"
        "-framework AVFoundation"
    )
endif()

if(MELLO_HAS_DAV1D)
    if(TARGET PkgConfig::DAV1D)
        target_link_libraries(mello PRIVATE PkgConfig::DAV1D)
    elseif(TARGET dav1d::dav1d)
        target_link_libraries(mello PRIVATE dav1d::dav1d)
    endif()
endif()

# Download Silero VAD v6.2 model if missing
set(SILERO_VAD_MODEL "${CMAKE_CURRENT_SOURCE_DIR}/models/silero_vad.onnx")
if(NOT EXISTS "${SILERO_VAD_MODEL}")
    message(STATUS "[mello] Downloading Silero VAD v6.2 model...")
    file(MAKE_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/models")
    file(DOWNLOAD
        "https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.1/src/silero_vad/data/silero_vad.onnx"
        "${SILERO_VAD_MODEL}"
        SHOW_PROGRESS
        STATUS SILERO_DL_STATUS)
    list(GET SILERO_DL_STATUS 0 SILERO_DL_CODE)
    if(NOT SILERO_DL_CODE EQUAL 0)
        list(GET SILERO_DL_STATUS 1 SILERO_DL_MSG)
        message(FATAL_ERROR "[mello] Silero VAD download failed: ${SILERO_DL_MSG}")
    endif()
    message(STATUS "[mello] Silero VAD model downloaded")
endif()

install(TARGETS mello rnnoise
    ARCHIVE DESTINATION lib
    LIBRARY DESTINATION lib
    RUNTIME DESTINATION bin
)
install(FILES include/mello.h DESTINATION include)
install(FILES models/silero_vad.onnx DESTINATION models)

# Tests (opt-in via MELLO_BUILD_TESTS)
option(MELLO_BUILD_TESTS "Build libmello C++ tests" OFF)
if(MELLO_BUILD_TESTS)
    enable_testing()
    add_subdirectory(tests)
endif()
