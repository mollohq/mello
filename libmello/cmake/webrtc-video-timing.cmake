# ============================================================================
# Video receive timing + frame buffer (viewer jitter buffer, option
# MELLO_WEBRTC_VIDEO_TIMING)
# ============================================================================
# Source: https://webrtc.googlesource.com/src refs/branch-heads/6778 (M131),
# commit 79aff54b0fa9238ce3518dd9eaf9610cd6f22e82, the same milestone as the
# APM in the submodule and the NetEQ overlay. The files live in their own
# overlay tree, third_party/webrtc-video-timing/webrtc, with the same rule as
# the NetEQ overlay: it holds only files that the submodule and the NetEQ
# overlay do not have. All three trees share one include root layout.
#
# Contents: modules/video_coding/timing (VCMTiming, JitterEstimator and their
# filters), api/video/frame_buffer (FrameBuffer, the M131 successor of
# frame_buffer2), video/frame_decode_timing (the release decision), and the
# EncodedFrame/EncodedImage types that FrameBuffer stores. No RTP
# depacketization, no decoder, no transport.
#
# The file is included from the end of
# cmake/webrtc-audio-processing/CMakeLists.txt, so WAP_ROOT, WAP_DEFINES and
# the webrtc_neteq target are in scope. It links webrtc_neteq for the shared
# rtc_base, system_wrappers (Clock) and struct_parameters_parser objects.
option(MELLO_WEBRTC_VIDEO_TIMING "Build the vendored WebRTC video timing and frame buffer" ON)

if(MELLO_WEBRTC_VIDEO_TIMING)
    if(NOT MELLO_WEBRTC_NETEQ)
        message(FATAL_ERROR
            "MELLO_WEBRTC_VIDEO_TIMING needs MELLO_WEBRTC_NETEQ: the video timing "
            "target links the shared rtc_base and Clock objects of webrtc_neteq.")
    endif()

    set(VIDEO_TIMING_ROOT ${CMAKE_CURRENT_LIST_DIR}/../third_party/webrtc-video-timing)

    set(VIDEO_TIMING_SOURCES
        webrtc/api/video/encoded_frame.cc
        webrtc/api/video/encoded_image.cc
        webrtc/api/video/frame_buffer.cc
        webrtc/modules/video_coding/include/video_codec_interface.cc
        webrtc/modules/video_coding/timing/decode_time_percentile_filter.cc
        webrtc/modules/video_coding/timing/frame_delay_variation_kalman_filter.cc
        webrtc/modules/video_coding/timing/inter_frame_delay_variation_calculator.cc
        webrtc/modules/video_coding/timing/jitter_estimator.cc
        webrtc/modules/video_coding/timing/rtt_filter.cc
        webrtc/modules/video_coding/timing/timestamp_extrapolator.cc
        webrtc/modules/video_coding/timing/timing.cc
        webrtc/modules/video_coding/utility/decoded_frames_history.cc
        webrtc/video/frame_decode_timing.cc
    )
    list(TRANSFORM VIDEO_TIMING_SOURCES PREPEND "${VIDEO_TIMING_ROOT}/")

    add_library(webrtc_video_timing STATIC ${VIDEO_TIMING_SOURCES})
    # frame_decode_timing.cc uses designated initializers: C++20, as on the
    # APM and NetEQ targets.
    set_target_properties(webrtc_video_timing PROPERTIES CXX_STANDARD 20 CXX_STANDARD_REQUIRED ON)
    target_compile_definitions(webrtc_video_timing PRIVATE ${WAP_DEFINES})
    target_include_directories(webrtc_video_timing BEFORE PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/compat)
    target_include_directories(webrtc_video_timing PUBLIC ${VIDEO_TIMING_ROOT}/webrtc)
    target_link_libraries(webrtc_video_timing
        PUBLIC
            webrtc_neteq
            absl::inlined_vector
            absl::variant
            absl::any_invocable
    )
    if(MSVC)
        # Same list as the NetEQ target.
        target_compile_options(webrtc_video_timing PRIVATE
            /wd4244 /wd4267 /wd4305 /wd4838 /wd4065 /wd4702 /wd4018
            /wd4334 /wd4996 /wd4100 /wd4127 /wd4189 /wd4456 /wd4715
        )
    else()
        target_compile_options(webrtc_video_timing PRIVATE
            -Wno-unused-parameter -Wno-sign-compare -Wno-missing-field-initializers
        )
    endif()
endif()
