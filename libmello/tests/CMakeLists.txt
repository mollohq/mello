find_package(GTest CONFIG REQUIRED)

add_executable(mello_tests
    test_opus_codec.cpp
    test_noise_suppressor.cpp
    test_echo_canceller.cpp
    test_jitter_buffer.cpp
    test_vad.cpp
    test_video_pipeline.cpp
)

target_link_libraries(mello_tests PRIVATE
    mello
    GTest::gtest
    GTest::gtest_main
)

target_include_directories(mello_tests PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/../src
    ${CMAKE_CURRENT_SOURCE_DIR}/../include
)

include(GoogleTest)
gtest_discover_tests(mello_tests)
