cmake_minimum_required(VERSION 3.20)
project(mello VERSION 0.1.0 LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

# Dependencies via vcpkg
find_package(Opus CONFIG REQUIRED)
find_package(LibDataChannel CONFIG REQUIRED)

# ONNX Runtime - use prebuilt shared library (static is 2GB+ and impractical)
set(ORT_DIR ${CMAKE_CURRENT_SOURCE_DIR}/third_party/onnxruntime/onnxruntime-win-x64-1.23.2)
add_library(onnxruntime SHARED IMPORTED)
set_target_properties(onnxruntime PROPERTIES
    IMPORTED_LOCATION "${ORT_DIR}/lib/onnxruntime.dll"
    IMPORTED_IMPLIB "${ORT_DIR}/lib/onnxruntime.lib"
    INTERFACE_INCLUDE_DIRECTORIES "${ORT_DIR}/include"
)

# Build rnnoise from source (not available on Windows via vcpkg)
set(RNNOISE_DIR ${CMAKE_CURRENT_SOURCE_DIR}/third_party/rnnoise)
set(RNNOISE_SOURCES
    ${RNNOISE_DIR}/src/denoise.c
    ${RNNOISE_DIR}/src/rnn.c
    ${RNNOISE_DIR}/src/rnnoise_tables.c
    ${RNNOISE_DIR}/src/rnnoise_data.c
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

# Sources
set(MELLO_SOURCES
    src/mello.cpp
    src/context.cpp
    src/audio/audio_pipeline.cpp
    src/audio/capture_wasapi.cpp
    src/audio/playback_wasapi.cpp
    src/audio/opus_codec.cpp
    src/audio/noise_suppressor.cpp
    src/audio/jitter_buffer.cpp
    src/audio/vad.cpp
    src/transport/peer_connection.cpp
)

if(WIN32)
    list(APPEND MELLO_SOURCES src/audio/device_enumerator_wasapi.cpp)
else()
    list(APPEND MELLO_SOURCES src/audio/device_enumerator_stub.cpp)
endif()

# Static library for Rust FFI linking
add_library(mello STATIC ${MELLO_SOURCES})

target_compile_definitions(mello PRIVATE MELLO_EXPORTS NOMINMAX MELLO_HAS_ONNX)

if(MSVC)
    target_compile_options(mello PRIVATE /EHsc)
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
    )
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
