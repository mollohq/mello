find_package(GTest CONFIG REQUIRED)

add_executable(mello_tests
    test_opus_codec.cpp
    test_clip_audio_stream.cpp
    test_clip_buffer.cpp
    test_clip_player.cpp
    test_replay_load.cpp
    test_replay_mix.cpp
    test_replay_mux.cpp
    test_mp4.cpp
    test_replay_ring.cpp
    test_noise_suppressor.cpp
    test_echo_canceller.cpp
    test_echo_suppressor.cpp
    test_audio_pipeline.cpp
    test_vpio_duplex.cpp
    test_voice_playout.cpp
    test_voice_receive.cpp
    test_voice_slot_hold.cpp
    test_voice_capture.cpp
    test_speech_gate.cpp
    test_voice_packet_sink.cpp
    test_send_adaptation.cpp
    test_vad.cpp
    test_video_pipeline.cpp
    test_video_viewer.cpp
    test_stream_audio_pipeline.cpp
    test_d3d9ex_share.cpp
    test_hook_capture.cpp
    test_hook_loopback.cpp
    test_graphics_device.cpp
    test_process_liveness.cpp
    test_capture_wgc_restart.cpp
    test_window_crop.cpp
    test_video_preprocessor.cpp
    test_pipewire_metadata.cpp
    test_yuv_convert.cpp
    test_openh264_linux_decoder.cpp
    test_linux_decoders.cpp
)

# Separate target: process enumeration coverage, independent of mello_tests.
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
# The speech corpus of the voice quality gate, for receive tests that need
# real speech (Opus in-band FEC is coded for speech, not for a tone).
target_compile_definitions(mello_tests PRIVATE
    MELLO_VOICE_FIXTURE_DIR="${CMAKE_CURRENT_SOURCE_DIR}/fixtures/voice"
    # A short H.264 stream for the game clip mux tests (test_replay_mux.cpp).
    MELLO_REPLAY_FIXTURE_DIR="${CMAKE_CURRENT_SOURCE_DIR}/fixtures/replay"
    # H.264 streams for the Media Foundation decoder tests (test_windows_decoders.cpp).
    MELLO_VIDEO_FIXTURE_DIR="${CMAKE_CURRENT_SOURCE_DIR}/fixtures/video"
)

# The NVENC, AMF and oneVPL headers, for the cost-tier tables of the encoders
# (test_video_pipeline.cpp). Headers only: nothing links.
if(WIN32 AND NOT IOS)
    target_include_directories(mello_tests PRIVATE ${MELLO_NV_CODEC_DIR} ${MELLO_AMF_DIR} ${MELLO_VPL_DIR})
endif()

# The D3D9Ex share spike links d3d9 directly. Test-only: production code
# opens the shared handle from D3D11 and never touches D3D9.
if(WIN32)
    target_link_libraries(mello_tests PRIVATE d3d9)
endif()

target_include_directories(mello_tests PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/../src
    ${CMAKE_CURRENT_SOURCE_DIR}/../include
    # The OpenH264 v2.6.0 API headers and the NVDEC headers, for the Linux
    # decoder round trips. Headers only.
    ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/openh264/include
    ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/nv-codec/include
    # The game capture hook's protocol header, shared with the hook project.
    ${CMAKE_CURRENT_SOURCE_DIR}/../../hook/include
    # Nullability compat shims for the vendored WAP tree (see
    # cmake/webrtc-audio-processing/compat). Test TUs include
    # echo_canceller.hpp -> api/scoped_refptr.h directly, so they need the
    # same shadow the mello target gets (PRIVATE there does not propagate).
    ${CMAKE_CURRENT_SOURCE_DIR}/../cmake/webrtc-audio-processing/compat
)

add_executable(mello_rtp_tests
    test_rtp_h264_receiver.cpp
    test_rtp_video_receiver_session.cpp
    test_video_receive_timeline.cpp
    test_av_sync.cpp
    test_rtp_video_sender.cpp
    test_peer_rtp_roles.cpp
    test_voice_rtp_timestamps.cpp
    test_voice_send_feedback.cpp
    test_twcc.cpp
    test_send_controller.cpp
    test_ulpfec.cpp
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

if(CMAKE_VERSION VERSION_LESS 3.22)
    message(FATAL_ERROR "libmello tests need CMake 3.22 or newer: gtest_discover_tests(TEST_FILTER) is used to split serial and parallel suites")
endif()

# ctest can run the cases in parallel (ctest -j): gtest_discover_tests makes
# one ctest entry, and one process, per case. The suites below use a real
# resource that a second test process can hold at the same time, or measure
# wall-clock rates that a loaded machine skews. They run with RUN_SERIAL
# (no other case runs at the same time), under the name prefix "serial/".
# Put a new suite here when it opens one of these:
#   - D3D11/DXGI devices, screen and window capture, the game hook and its
#     injected test processes, hardware encoders and decoders (NVENC, AMF,
#     QSV, NVDEC, VA-API), the streaming reference loopback;
#   - process enumeration and process start/stop;
#   - audio devices (Core Audio HAL, VoiceProcessingIO);
#   - sockets: libdatachannel peers and ICE on loopback, mello_init.
set(MELLO_SERIAL_SUITES
    # GPU, capture, hook, hardware codecs (Windows)
    GraphicsDevice GraphicsDeviceTest HookCapture HookLoopback D3D9ExShare
    WgcCaptureRestart VideoPipelineTest CaptureLadder NativePresent
    NvdecHostCopy EncodeSize EncoderControl EncoderFactory EncoderCostTier
    EncoderOverloadPolicy FramerateDecimation IdleKeepalive
    VideoPipelinePreprocessor VideoPipelineCursorShape VideoPipelineFit
    ViewerPipelineTest ViewerConverter
    ReferenceBitrate ReferenceClip ReferenceGeometry ReferenceLoopback
    SoftCodecSwitch ViewerSizeChange
    *MfDecoderTest WindowsDecoderFactory WindowsViewerDecoderSwitch WindowsVideoDevice MfFixtures
    # GPU and hardware codecs (Linux)
    LinuxGpu LinuxVideoGpu LinuxVaDriver VaDmaBufFormats ViewerDecoderSwitch
    LinuxHardwareDecoder LinuxHardwareEncoder
    # Processes
    HookPolicy ProcessEnum ProcessLiveness
    # Audio devices (macOS)
    PipelineBackend VpioDuplex
    # Sockets and ICE
    PeerRtpRolesTest AvSyncWire RtpVideoSender*Test VoicePacketSinkWire
    VoiceRtpTimestamps VoiceSendFeedbackWire VoiceSlotHoldTest StreamAudioCApi
)
list(TRANSFORM MELLO_SERIAL_SUITES APPEND ".*")
list(JOIN MELLO_SERIAL_SUITES ":" MELLO_SERIAL_FILTER)

function(mello_discover_tests target)
    gtest_discover_tests(${target}
        TEST_FILTER "${MELLO_SERIAL_FILTER}"
        TEST_PREFIX "serial/"
        PROPERTIES RUN_SERIAL TRUE)
    gtest_discover_tests(${target}
        TEST_FILTER "-${MELLO_SERIAL_FILTER}")
endfunction()

mello_discover_tests(mello_tests)
mello_discover_tests(mello_rtp_tests)
mello_discover_tests(mello_procenum_tests)

# The reference source and the software encoder of the streaming quality
# gate are Windows only (D3D11). The loopback needs the Cisco OpenH264 DLL,
# which scripts/stream-gate.sh fetches into third_party/openh264; it is
# copied next to the tests when present, else the loopback skips.
if(APPLE AND NOT IOS)
    target_sources(mello_tests PRIVATE test_capture_manager.cpp test_encoder_videotoolbox.cpp)
endif()
if(WIN32 AND NOT IOS)
    target_sources(mello_tests PRIVATE test_capture_reference.cpp test_capture_manager.cpp
                                       test_windows_decoders.cpp)
    set(_oh264_dll "${CMAKE_CURRENT_SOURCE_DIR}/../third_party/openh264/openh264-2.6.0-win64.dll")
    if(EXISTS "${_oh264_dll}")
        add_custom_command(TARGET mello_tests POST_BUILD
            COMMAND ${CMAKE_COMMAND} -E copy_if_different
                "${_oh264_dll}" "$<TARGET_FILE_DIR:mello_tests>/openh264-2.6.0-win64.dll"
            COMMENT "Copying the OpenH264 DLL next to mello_tests")
    endif()
endif()

# Windows: ORT loads onnxruntime.dll at runtime via LoadLibrary. Copy it
# next to the test binaries so ctest passes without manual PATH setup.
# Desktop-only: iOS links ORT statically and has no DLLs.
if(WIN32 AND NOT IOS)
    foreach(_ort_dll onnxruntime.dll onnxruntime_providers_shared.dll)
        add_custom_command(TARGET mello_tests POST_BUILD
            COMMAND ${CMAKE_COMMAND} -E copy_if_different
                "${ORT_DIR}/lib/${_ort_dll}"
                "$<TARGET_FILE_DIR:mello_tests>/${_ort_dll}"
            COMMENT "Copying ${_ort_dll} next to mello_tests")
    endforeach()
endif()

# The ONNX models, into models/ next to mello_tests: mello_init loads them
# through find_model_file() (audio_pipeline.cpp), which looks in
# <exe dir>/models/ before it walks up to libmello/models/. The walk finds
# nothing when the build dir is outside the repo. Only mello_tests runs the
# audio pipeline; the other test executables do not load the models.
# Desktop-only: iOS stubs VAD and the suppressor, and CMake does not
# download the models there.
if(NOT IOS)
    add_custom_command(TARGET mello_tests POST_BUILD
        COMMAND ${CMAKE_COMMAND} -E make_directory
            "$<TARGET_FILE_DIR:mello_tests>/models"
        COMMAND ${CMAKE_COMMAND} -E copy_if_different
            "${SILERO_VAD_MODEL}" "${ECHO_SUPPRESSOR_MODEL}"
            "$<TARGET_FILE_DIR:mello_tests>/models/"
        COMMENT "Copying the ONNX models next to mello_tests")
endif()

# Stage 3 spike: vendored NetEQ + AudioMixerImpl (option MELLO_WEBRTC_NETEQ,
# cmake/webrtc-audio-processing). Own executable: it replaces the global
# operator new to count heap per NetEQ instance, and it does not link the
# mello library. It compiles opus_codec.cpp for the project's OpusEnc.
if(MELLO_WEBRTC_NETEQ)
    add_executable(mello_neteq_spike_tests
        test_neteq_spike.cpp
        ${CMAKE_CURRENT_SOURCE_DIR}/../src/audio/opus_codec.cpp
    )
    set_target_properties(mello_neteq_spike_tests PROPERTIES CXX_STANDARD 20 CXX_STANDARD_REQUIRED ON)
    target_link_libraries(mello_neteq_spike_tests PRIVATE
        webrtc_neteq
        GTest::gtest
        GTest::gtest_main
    )
    if(WIN32)
        target_link_libraries(mello_neteq_spike_tests PRIVATE psapi)
    endif()
    target_include_directories(mello_neteq_spike_tests PRIVATE
        ${CMAKE_CURRENT_SOURCE_DIR}/../src
        ${CMAKE_CURRENT_SOURCE_DIR}/../cmake/webrtc-audio-processing/compat
    )
    target_compile_definitions(mello_neteq_spike_tests PRIVATE
        MELLO_VOICE_FIXTURE_DIR="${CMAKE_CURRENT_SOURCE_DIR}/fixtures/voice"
    )
    if(WIN32)
        # rtc_base headers include winsock2.h; windows.h must not pull winsock.h.
        target_compile_definitions(mello_neteq_spike_tests PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN)
    endif()
    gtest_discover_tests(mello_neteq_spike_tests)
endif()

# Stage 3 streaming spike: vendored video receive timing and frame buffer
# (option MELLO_WEBRTC_VIDEO_TIMING, cmake/webrtc-video-timing.cmake). Own
# executable: it replaces the global operator new to count heap per timeline,
# and it does not link the mello library.
if(MELLO_WEBRTC_VIDEO_TIMING)
    add_executable(mello_video_timing_spike_tests test_video_timing_spike.cpp)
    set_target_properties(mello_video_timing_spike_tests PROPERTIES CXX_STANDARD 20 CXX_STANDARD_REQUIRED ON)
    target_link_libraries(mello_video_timing_spike_tests PRIVATE
        webrtc_video_timing
        GTest::gtest
        GTest::gtest_main
    )
    target_include_directories(mello_video_timing_spike_tests PRIVATE
        ${CMAKE_CURRENT_SOURCE_DIR}/../cmake/webrtc-audio-processing/compat
    )
    if(WIN32)
        # rtc_base headers include winsock2.h; windows.h must not pull winsock.h.
        target_compile_definitions(mello_video_timing_spike_tests PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN)
    endif()
    gtest_discover_tests(mello_video_timing_spike_tests)
endif()


# Game clips: a manual probe of WASAPI process loopback on a running
# game (CLIPS.md 15.12). Not a test; ctest does not run it.
if(WIN32)
    add_executable(mello_loopback_probe loopback_probe.cpp)
    target_include_directories(mello_loopback_probe PRIVATE
        ${CMAKE_CURRENT_SOURCE_DIR}/../src
        ${CMAKE_CURRENT_SOURCE_DIR}/../include
    )
    target_compile_definitions(mello_loopback_probe PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN)
    target_link_libraries(mello_loopback_probe PRIVATE mello ole32)

    # The real loopback row of the stream gate's replay lane: a 1 kHz tone in
    # bursts, with each onset on the steady clock (CLIPS.md 15.13).
    # Not a test; ctest does not run it.
    add_executable(mello_tone_player tone_player.cpp)
    target_compile_definitions(mello_tone_player PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN)
    target_link_libraries(mello_tone_player PRIVATE ole32)
endif()

# Game clips on macOS (CLIPS.md 15.12a): which capture gives the
# game's sound only. A manual probe, not a test; ctest does not run it.
if(APPLE AND NOT IOS)
    add_executable(mello_sck_audio_probe mello_sck_audio_probe.mm)
    set_source_files_properties(mello_sck_audio_probe.mm PROPERTIES COMPILE_FLAGS "-fobjc-arc -fmodules")
    target_link_libraries(mello_sck_audio_probe PRIVATE
        "-framework AppKit" "-framework AudioToolbox" "-framework CoreAudio" "-framework CoreMedia"
        "-framework ScreenCaptureKit" "-framework Foundation")
endif()
