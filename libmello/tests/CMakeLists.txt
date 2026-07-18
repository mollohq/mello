find_package(GTest CONFIG REQUIRED)

add_executable(mello_tests
    test_opus_codec.cpp
    test_noise_suppressor.cpp
    test_echo_canceller.cpp
    test_jitter_buffer.cpp
    test_vad.cpp
    test_video_pipeline.cpp
)

# Separate target: mello_tests currently fails to build (test_jitter_buffer.cpp
# predates the JitterPopResult API change) — keep process-enum coverage running
# until that suite is repaired.
add_executable(mello_procenum_tests
    test_process_enum.cpp
)

target_link_libraries(mello_procenum_tests PRIVATE
    mello
    GTest::gtest
    GTest::gtest_main
)

target_include_directories(mello_procenum_tests PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/../src
    ${CMAKE_CURRENT_SOURCE_DIR}/../include
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

add_executable(mello_rtp_tests
    test_rtp_h264_receiver.cpp
    test_rtp_video_receiver_session.cpp
    test_rtp_video_sender.cpp
    test_peer_rtp_roles.cpp
)

target_link_libraries(mello_rtp_tests PRIVATE
    mello
    GTest::gtest
    GTest::gtest_main
)

target_include_directories(mello_rtp_tests PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/../src
    ${CMAKE_CURRENT_SOURCE_DIR}/../include
)

if(MSVC)
    target_compile_options(mello_rtp_tests PRIVATE /W4 /WX)
    set_property(TARGET mello_rtp_tests PROPERTY
        MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL")
    set_property(TARGET mello PROPERTY
        MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL")
    target_link_options(mello_rtp_tests PRIVATE
        "$<$<CONFIG:Release>:/NODEFAULTLIB:MSVCRTD>"
        "$<$<CONFIG:Release>:/NODEFAULTLIB:LIBCMTD>")
endif()

include(GoogleTest)
gtest_discover_tests(mello_tests)
gtest_discover_tests(mello_rtp_tests)
gtest_discover_tests(mello_procenum_tests)
