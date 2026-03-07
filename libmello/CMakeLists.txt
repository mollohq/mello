cmake_minimum_required(VERSION 3.20)
project(mello VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

# Options
option(MELLO_BUILD_TESTS "Build tests" ON)
option(MELLO_ENABLE_NVENC "Enable NVIDIA NVENC" ON)
option(MELLO_ENABLE_AMF "Enable AMD AMF" ON)
option(MELLO_ENABLE_QSV "Enable Intel Quick Sync" ON)

# Find dependencies
# TODO: Add find_package calls for dependencies

# Sources
set(MELLO_SOURCES
    src/mello.cpp
    src/context.cpp
    src/audio/audio_pipeline.cpp
    src/video/video_pipeline.cpp
    src/transport/peer_connection.cpp
)

# Library
add_library(mello SHARED ${MELLO_SOURCES})

target_include_directories(mello
    PUBLIC include
    PRIVATE src
)

# Windows-specific
if(WIN32)
    target_link_libraries(mello PRIVATE
        d3d11
        dxgi
        mfplat
        mfuuid
        ole32
        winmm
    )
endif()

# Install
install(TARGETS mello RUNTIME DESTINATION bin LIBRARY DESTINATION lib)
install(FILES include/mello.h DESTINATION include)

# Tests
if(MELLO_BUILD_TESTS)
    enable_testing()
    # TODO: Add test executables
endif()
