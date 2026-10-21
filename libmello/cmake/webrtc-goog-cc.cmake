# ============================================================================
# goog_cc send-side congestion controller (option MELLO_WEBRTC_GOOG_CC)
# ============================================================================
# Included at the end of cmake/webrtc-audio-processing/CMakeLists.txt, so it
# reuses WAP_DEFINES, WAP_ROOT and the webrtc_neteq target from that file.
#
# Source: https://webrtc.googlesource.com/src refs/branch-heads/6778 (M131),
# commit 79aff54b0fa9238ce3518dd9eaf9610cd6f22e82, the same milestone as the
# APM submodule and the NetEQ overlay. The files live in their own overlay
# tree, third_party/webrtc-goog-cc/webrtc, with the upstream path layout. The
# overlay holds only files that the APM submodule and the NetEQ overlay do not
# have: rtc_base, api/units, the field trial parser, the environment and the
# RTC event log interface come from those trees (byte-identical at M131).
# Upstream files are not edited. The RTC event log encoder is not vendored:
# the goog_cc event classes are stubs in cmake/webrtc-goog-cc/compat.
#
# Contents: modules/congestion_controller/goog_cc, the remote_bitrate_estimator
# pieces it uses (AIMD rate control, inter arrival, overuse detector),
# modules/pacing/interval_budget (the ALR detector budget, not the pacer),
# api/transport network types and the NetworkControllerInterface, and the
# rate control and ALR experiment parsers. The video codec headers under
# api/video_codecs and video/config are header-only inputs of
# rate_control_settings.h; none of their sources are compiled.
#
# Stage 4 spike: only tests/test_goog_cc_spike.cpp links this target.
# Apple clang is unverified: OFF on Apple until a Mac build runs, then stage 4 turns it on.
if(APPLE)
    set(MELLO_WEBRTC_GOOG_CC_DEFAULT OFF)
else()
    set(MELLO_WEBRTC_GOOG_CC_DEFAULT ON)
endif()
option(MELLO_WEBRTC_GOOG_CC "Build the vendored WebRTC goog_cc congestion controller" ${MELLO_WEBRTC_GOOG_CC_DEFAULT})

if(MELLO_WEBRTC_GOOG_CC)
    if(NOT MELLO_WEBRTC_NETEQ)
        message(FATAL_ERROR "[mello] MELLO_WEBRTC_GOOG_CC needs MELLO_WEBRTC_NETEQ=ON (shared WebRTC base files)")
    endif()

    set(GOOG_CC_ROOT ${CMAKE_CURRENT_LIST_DIR}/../third_party/webrtc-goog-cc)

    set(GOOG_CC_SOURCES
        webrtc/api/transport/goog_cc_factory.cc
        webrtc/api/transport/network_types.cc
        webrtc/modules/congestion_controller/goog_cc/acknowledged_bitrate_estimator.cc
        webrtc/modules/congestion_controller/goog_cc/acknowledged_bitrate_estimator_interface.cc
        webrtc/modules/congestion_controller/goog_cc/alr_detector.cc
        webrtc/modules/congestion_controller/goog_cc/bitrate_estimator.cc
        webrtc/modules/congestion_controller/goog_cc/congestion_window_pushback_controller.cc
        webrtc/modules/congestion_controller/goog_cc/delay_based_bwe.cc
        webrtc/modules/congestion_controller/goog_cc/goog_cc_network_control.cc
        webrtc/modules/congestion_controller/goog_cc/inter_arrival_delta.cc
        webrtc/modules/congestion_controller/goog_cc/link_capacity_estimator.cc
        webrtc/modules/congestion_controller/goog_cc/loss_based_bandwidth_estimation.cc
        webrtc/modules/congestion_controller/goog_cc/loss_based_bwe_v2.cc
        webrtc/modules/congestion_controller/goog_cc/probe_bitrate_estimator.cc
        webrtc/modules/congestion_controller/goog_cc/probe_controller.cc
        webrtc/modules/congestion_controller/goog_cc/robust_throughput_estimator.cc
        webrtc/modules/congestion_controller/goog_cc/send_side_bandwidth_estimation.cc
        webrtc/modules/congestion_controller/goog_cc/trendline_estimator.cc
        webrtc/modules/pacing/interval_budget.cc
        webrtc/modules/remote_bitrate_estimator/aimd_rate_control.cc
        webrtc/modules/remote_bitrate_estimator/bwe_defines.cc
        webrtc/modules/remote_bitrate_estimator/inter_arrival.cc
        webrtc/modules/remote_bitrate_estimator/overuse_detector.cc
        webrtc/rtc_base/experiments/alr_experiment.cc
        webrtc/rtc_base/experiments/field_trial_list.cc
        webrtc/rtc_base/experiments/rate_control_settings.cc
    )
    list(TRANSFORM GOOG_CC_SOURCES PREPEND "${GOOG_CC_ROOT}/")

    add_library(webrtc_goog_cc STATIC ${GOOG_CC_SOURCES})
    set_target_properties(webrtc_goog_cc PROPERTIES CXX_STANDARD 20 CXX_STANDARD_REQUIRED ON)
    # WAP_DEFINES without WEBRTC_ENABLE_SYMBOL_EXPORT. With it, RTC_EXPORT is
    # dllexport on Windows, and MSVC then emits every inline member of an
    # exported class. rate_control_settings.h includes the video codec
    # headers, and their exported inline comparison operators call
    # SdpVideoFormat and SimulcastStream code that is not vendored. The
    # target is static, so it has nothing to export.
    set(GOOG_CC_DEFINES ${WAP_DEFINES})
    list(REMOVE_ITEM GOOG_CC_DEFINES WEBRTC_ENABLE_SYMBOL_EXPORT)
    target_compile_definitions(webrtc_goog_cc PRIVATE ${GOOG_CC_DEFINES})
    # The event stubs replace the upstream event headers. PRIVATE: the
    # goog_cc public headers do not include them.
    target_include_directories(webrtc_goog_cc BEFORE PRIVATE
        ${CMAKE_CURRENT_LIST_DIR}/webrtc-goog-cc/compat
        ${CMAKE_CURRENT_SOURCE_DIR}/compat
    )
    target_include_directories(webrtc_goog_cc PUBLIC ${GOOG_CC_ROOT}/webrtc)
    # webrtc_neteq carries the platform macros (PUBLIC) and links the APM
    # target, which holds rtc_base, api/units and system_wrappers.
    target_link_libraries(webrtc_goog_cc PUBLIC webrtc_neteq)
    if(MSVC)
        # Same list as the NetEQ target.
        target_compile_options(webrtc_goog_cc PRIVATE
            /wd4244 /wd4267 /wd4305 /wd4838 /wd4065 /wd4702 /wd4018
            /wd4334 /wd4996 /wd4100 /wd4127 /wd4189 /wd4456 /wd4715
        )
    else()
        target_compile_options(webrtc_goog_cc PRIVATE
            -Wno-unused-parameter -Wno-sign-compare -Wno-missing-field-initializers
        )
    endif()
endif()
