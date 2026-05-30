cmake_minimum_required(VERSION 3.20)
project(mello VERSION 0.1.0 LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

if(APPLE AND NOT CMAKE_OSX_DEPLOYMENT_TARGET)
    set(CMAKE_OSX_DEPLOYMENT_TARGET "15.0" CACHE STRING "Minimum macOS version")
endif()

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

# ONNX Runtime. Desktop: prebuilt shared lib, auto-downloaded. iOS: pinned prebuilt
# *static* onnxruntime.xcframework (Microsoft official onnxruntime-c pod-archive),
# auto-downloaded + SHA256-verified (IOS-LIBMELLO-PORT §1a Step 2 / §4.3).
if(IOS)
    set(ORT_IOS_VERSION "1.23.0")  # nearest iOS pod to desktop 1.23.2 (same minor)
    set(ORT_IOS_SHA256 "756a78e0168f29840bc614b43aeb03e63673f44022e0221d21698a2c8ed747ef")
    set(ORT_IOS_BASE "${CMAKE_CURRENT_SOURCE_DIR}/third_party/onnxruntime-ios")
    set(ORT_IOS_XCFW "${ORT_IOS_BASE}/onnxruntime.xcframework")
    if(NOT EXISTS "${ORT_IOS_XCFW}")
        set(ORT_IOS_ZIP "${CMAKE_CURRENT_BINARY_DIR}/pod-archive-onnxruntime-c-${ORT_IOS_VERSION}.zip")
        if(NOT EXISTS "${ORT_IOS_ZIP}")
            message(STATUS "[mello] Downloading ONNX Runtime iOS xcframework v${ORT_IOS_VERSION}...")
            file(DOWNLOAD
                "https://download.onnxruntime.ai/pod-archive-onnxruntime-c-${ORT_IOS_VERSION}.zip"
                "${ORT_IOS_ZIP}"
                EXPECTED_HASH SHA256=${ORT_IOS_SHA256}
                SHOW_PROGRESS STATUS ORT_DL_STATUS)
            list(GET ORT_DL_STATUS 0 ORT_DL_CODE)
            if(NOT ORT_DL_CODE EQUAL 0)
                list(GET ORT_DL_STATUS 1 ORT_DL_MSG)
                message(FATAL_ERROR "[mello] ONNX Runtime iOS download failed: ${ORT_DL_MSG}")
            endif()
        endif()
        file(ARCHIVE_EXTRACT INPUT "${ORT_IOS_ZIP}" DESTINATION "${ORT_IOS_BASE}")
        message(STATUS "[mello] ONNX Runtime iOS xcframework extracted to ${ORT_IOS_BASE}")
    endif()
    # Device slice is arm64; simulator slice is fat (arm64+x86_64). The framework
    # binary is a static ar archive. MELLO_IOS_SIMULATOR is passed by build.rs.
    if(MELLO_IOS_SIMULATOR)
        set(ORT_DIR "${ORT_IOS_XCFW}/ios-arm64_x86_64-simulator/onnxruntime.framework")
    else()
        set(ORT_DIR "${ORT_IOS_XCFW}/ios-arm64/onnxruntime.framework")
    endif()
    if(NOT EXISTS "${ORT_DIR}/onnxruntime")
        message(FATAL_ERROR "[mello] ORT iOS slice not found at ${ORT_DIR}")
    endif()
    add_library(onnxruntime STATIC IMPORTED)
    set_target_properties(onnxruntime PROPERTIES
        IMPORTED_LOCATION "${ORT_DIR}/onnxruntime"
        INTERFACE_INCLUDE_DIRECTORIES "${ORT_DIR}/Headers"
    )
    message(STATUS "[mello] ONNX Runtime iOS v${ORT_IOS_VERSION} (static) at ${ORT_DIR}")
else()
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
endif() # IOS vs desktop (ONNX Runtime)

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

# WebRTC audio processing (AEC3 + AGC2) — CMake overlay for vendored submodule
add_subdirectory(cmake/webrtc-audio-processing)

# Platform-agnostic sources
set(MELLO_SOURCES
    src/mello.cpp
    src/context.cpp
    src/audio/audio_pipeline.cpp
    src/audio/clip_buffer.cpp
    src/audio/clip_encoder.cpp
    src/audio/opus_codec.cpp
    src/audio/noise_suppressor.cpp
    src/audio/echo_canceller.cpp
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
        src/audio/audio_session_win.cpp
        src/audio/device_enumerator_wasapi.cpp
        src/audio/clip_encoder_wmf.cpp
        src/audio/mic_permission_stub.cpp
    )
elseif(IOS)
    # RemoteIO capture/playback + AVAudioSession + mic permission (Step 3). Device
    # enumeration stays stubbed (iOS has one system-managed route, no picker) and
    # clip encode stays stubbed until its own step.
    list(APPEND MELLO_SOURCES
        src/audio/capture_remoteio.cpp
        src/audio/playback_remoteio.cpp
        src/audio/audio_backend_ios.cpp
        src/audio/audio_session_ios.mm
        src/audio/mic_permission_ios.mm
        src/audio/device_enumerator_ios_stub.cpp
        src/audio/clip_encoder_ios_stub.cpp
    )
elseif(APPLE)
    list(APPEND MELLO_SOURCES
        src/audio/capture_coreaudio.cpp
        src/audio/playback_coreaudio.cpp
        src/audio/audio_backend_coreaudio.cpp
        src/audio/device_enumerator_coreaudio.cpp
        src/audio/clip_encoder_audiotoolbox.cpp
        src/audio/mic_permission_macos.mm
    )
else()
    list(APPEND MELLO_SOURCES
        src/audio/audio_backend_stub.cpp
        src/audio/device_enumerator_stub.cpp
        src/audio/mic_permission_stub.cpp
    )
endif()

# Platform-specific video sources
if(WIN32)
    list(APPEND MELLO_SOURCES
        src/video/graphics_device.cpp
        src/video/video_pipeline.cpp
        src/video/capture_dxgi.cpp
        src/video/capture_wgc.cpp
        src/video/capture_process.cpp
        src/video/process_enum.cpp
        src/video/window_thumbnail.cpp
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
elseif(IOS)
    # Step 1: Metal graphics device + pipeline link, but decode/encode/capture are
    # stubbed (factories no-op via MELLO_IOS_NO_VIDEO). VideoToolbox decode lands in
    # IOS-LIBMELLO-PORT §1a Step 5. No hosting on iOS, so enumeration is stubbed.
    list(APPEND MELLO_SOURCES
        src/video/graphics_device_metal.mm
        src/video/video_pipeline.cpp
        src/video/process_enum_ios_stub.cpp
        src/video/window_thumbnail_ios_stub.cpp
    )
elseif(APPLE)
    list(APPEND MELLO_SOURCES
        src/video/graphics_device_metal.mm
        src/video/video_pipeline.cpp
        src/video/capture_screencapturekit.mm
        src/video/encoder_videotoolbox.mm
        src/video/decoder_videotoolbox.mm
        src/video/process_enum_macos.mm
        src/video/window_thumbnail_macos.mm
    )
else()
    list(APPEND MELLO_SOURCES
        src/video/graphics_device.cpp
        src/video/video_pipeline.cpp
        src/video/process_enum.cpp
        src/video/window_thumbnail.cpp
    )
endif()

# Enable ARC + modules for all ObjC++ sources.
# The code uses ARC constructs (__bridge_retained, no manual retain/release).
# Without -fobjc-arc, block-captured ObjC pointers aren't retained → dangling ptrs.
# -fmodules avoids SDK 26.2 header class-reference issues on older macOS.
if(APPLE)
    file(GLOB_RECURSE MELLO_MM_FILES "src/*.mm")
    set_source_files_properties(${MELLO_MM_FILES}
        PROPERTIES COMPILE_FLAGS "-fobjc-arc -fmodules"
    )
endif()

# Static library for Rust FFI linking
add_library(mello STATIC ${MELLO_SOURCES})

target_compile_definitions(mello PRIVATE MELLO_EXPORTS NOMINMAX)

# iOS gates: video decode/encode/capture stubbed (no VideoToolbox yet). VAD is
# live as of Step 2 (prebuilt static ORT). MELLO_IOS_NO_VIDEO drops with Step 5.
if(IOS)
    target_compile_definitions(mello PRIVATE MELLO_IOS MELLO_IOS_NO_VIDEO)
endif()

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
        webrtc_audio_processing
)
target_link_libraries(mello PUBLIC onnxruntime)

if(WIN32)
    target_link_libraries(mello PUBLIC
        ole32
        winmm
        ksuser
        mfplat
        mfreadwrite
        mfuuid
        avrt
        d3d11
        dxgi
        dxguid
        d3dcompiler
        windowsapp
    )
    # OpenH264 is NOT linked — Cisco prebuilt DLL loaded at runtime via LoadLibrary
elseif(IOS)
    # iOS: RemoteIO uses AudioToolbox/AVFoundation (no CoreAudio HAL); no
    # ScreenCaptureKit/AppKit (no hosting). VideoToolbox kept for Step 5.
    target_link_libraries(mello PUBLIC
        "-framework AudioToolbox"
        "-framework AVFoundation"
        "-framework CoreFoundation"
        "-framework Metal"
        "-framework VideoToolbox"
        "-framework CoreMedia"
        "-framework CoreVideo"
        "-framework CoreGraphics"
        "-framework Accelerate"
    )
elseif(APPLE)
    target_link_libraries(mello PUBLIC
        "-framework AudioToolbox"
        "-framework CoreAudio"
        "-framework CoreFoundation"
        "-framework AVFoundation"
        "-framework Metal"
        "-framework VideoToolbox"
        "-framework CoreMedia"
        "-framework CoreVideo"
        "-framework CoreGraphics"
        "-framework ScreenCaptureKit"
        "-framework AppKit"
        "-framework Accelerate"
    )
endif()

if(MELLO_HAS_DAV1D)
    if(TARGET PkgConfig::DAV1D)
        target_link_libraries(mello PRIVATE PkgConfig::DAV1D)
    elseif(TARGET dav1d::dav1d)
        target_link_libraries(mello PRIVATE dav1d::dav1d)
    endif()
endif()

# Download Silero VAD v6.2 model if missing (skipped on iOS — VAD stubbed in Step 1)
set(SILERO_VAD_MODEL "${CMAKE_CURRENT_SOURCE_DIR}/models/silero_vad.onnx")
if(NOT IOS AND NOT EXISTS "${SILERO_VAD_MODEL}")
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

install(TARGETS mello rnnoise webrtc_audio_processing
    ARCHIVE DESTINATION lib
    LIBRARY DESTINATION lib
    RUNTIME DESTINATION bin
)
install(FILES include/mello.h DESTINATION include)
if(NOT IOS)
    install(FILES models/silero_vad.onnx DESTINATION models)
endif()

# Tests (opt-in via MELLO_BUILD_TESTS)
option(MELLO_BUILD_TESTS "Build libmello C++ tests" OFF)
if(MELLO_BUILD_TESTS)
    enable_testing()
    add_subdirectory(tests)
endif()
