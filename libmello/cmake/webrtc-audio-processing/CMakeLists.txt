cmake_minimum_required(VERSION 3.20)
project(webrtc_audio_processing LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_C_STANDARD 11)

set(WAP_ROOT ${CMAKE_CURRENT_SOURCE_DIR}/../../third_party/webrtc-audio-processing)

# --- abseil-cpp (from vcpkg) ---
find_package(absl CONFIG REQUIRED)

# --- Platform detection ---
set(WAP_DEFINES
    WEBRTC_LIBRARY_IMPL
    WEBRTC_ENABLE_SYMBOL_EXPORT
    NDEBUG
    WEBRTC_APM_DEBUG_DUMP=0
)

set(WAP_LIBS)

if(WIN32)
    list(APPEND WAP_DEFINES WEBRTC_WIN _WIN32 __STDC_FORMAT_MACROS=1 NOMINMAX _USE_MATH_DEFINES)
    list(APPEND WAP_LIBS winmm)
elseif(APPLE)
    list(APPEND WAP_DEFINES WEBRTC_MAC WEBRTC_POSIX)
elseif(ANDROID)
    list(APPEND WAP_DEFINES WEBRTC_ANDROID WEBRTC_LINUX WEBRTC_POSIX)
else()
    list(APPEND WAP_DEFINES WEBRTC_LINUX WEBRTC_THREAD_RR WEBRTC_POSIX)
endif()

set(WAP_HAS_X86 FALSE)
set(WAP_HAS_AVX2 FALSE)
set(WAP_HAS_NEON FALSE)
set(WAP_HAS_ARM FALSE)

if(CMAKE_SYSTEM_PROCESSOR MATCHES "x86_64|AMD64|amd64|x86|i[3-6]86")
    set(WAP_HAS_X86 TRUE)
    set(WAP_HAS_AVX2 TRUE)
    list(APPEND WAP_DEFINES WEBRTC_ENABLE_AVX2)
elseif(CMAKE_SYSTEM_PROCESSOR MATCHES "aarch64|ARM64|arm64")
    set(WAP_HAS_NEON TRUE)
    list(APPEND WAP_DEFINES WEBRTC_ARCH_ARM64 WEBRTC_HAS_NEON)
elseif(CMAKE_SYSTEM_PROCESSOR MATCHES "arm")
    set(WAP_HAS_ARM TRUE)
    list(APPEND WAP_DEFINES WEBRTC_ARCH_ARM WEBRTC_ARCH_ARM_V7)
endif()

# ============================================================================
# Source file lists (translated from meson.build files)
# ============================================================================

# --- rtc_base ---
set(RTC_BASE_SOURCES
    webrtc/rtc_base/checks.cc
    webrtc/rtc_base/event.cc
    webrtc/rtc_base/event_tracer.cc
    webrtc/rtc_base/experiments/field_trial_parser.cc
    webrtc/rtc_base/logging.cc
    webrtc/rtc_base/memory/aligned_malloc.cc
    webrtc/rtc_base/platform_thread.cc
    webrtc/rtc_base/platform_thread_types.cc
    webrtc/rtc_base/race_checker.cc
    webrtc/rtc_base/string_encode.cc
    webrtc/rtc_base/string_to_number.cc
    webrtc/rtc_base/string_utils.cc
    webrtc/rtc_base/strings/string_builder.cc
    webrtc/rtc_base/synchronization/mutex.cc
    webrtc/rtc_base/synchronization/rw_lock_wrapper.cc
    webrtc/rtc_base/synchronization/yield.cc
    webrtc/rtc_base/synchronization/yield_policy.cc
    webrtc/rtc_base/system/file_wrapper.cc
    webrtc/rtc_base/time_utils.cc
    webrtc/rtc_base/zero_memory.cc
)
if(WIN32)
    list(APPEND RTC_BASE_SOURCES webrtc/rtc_base/synchronization/rw_lock_win.cc)
else()
    list(APPEND RTC_BASE_SOURCES webrtc/rtc_base/synchronization/rw_lock_posix.cc)
endif()

# --- api ---
set(API_SOURCES
    webrtc/api/audio/audio_frame.cc
    webrtc/api/audio/channel_layout.cc
    webrtc/api/audio/echo_canceller3_config.cc
    webrtc/api/audio_codecs/audio_decoder.cc
    webrtc/api/audio_codecs/audio_encoder.cc
    webrtc/api/rtp_headers.cc
    webrtc/api/rtp_packet_info.cc
    webrtc/api/task_queue/task_queue_base.cc
    webrtc/api/units/data_rate.cc
    webrtc/api/units/data_size.cc
    webrtc/api/units/frequency.cc
    webrtc/api/units/time_delta.cc
    webrtc/api/units/timestamp.cc
    webrtc/api/video/color_space.cc
    webrtc/api/video/hdr_metadata.cc
    webrtc/api/video/video_content_type.cc
    webrtc/api/video/video_timing.cc
)

# --- system_wrappers ---
set(SYSTEM_WRAPPERS_SOURCES
    webrtc/system_wrappers/source/cpu_features.cc
    webrtc/system_wrappers/source/field_trial.cc
    webrtc/system_wrappers/source/metrics.cc
    webrtc/system_wrappers/source/sleep.cc
)

# --- common_audio ---
set(COMMON_AUDIO_SOURCES
    webrtc/common_audio/audio_converter.cc
    webrtc/common_audio/audio_util.cc
    webrtc/common_audio/channel_buffer.cc
    webrtc/common_audio/fir_filter_c.cc
    webrtc/common_audio/fir_filter_factory.cc
    webrtc/common_audio/real_fourier.cc
    webrtc/common_audio/real_fourier_ooura.cc
    webrtc/common_audio/resampler/push_resampler.cc
    webrtc/common_audio/resampler/push_sinc_resampler.cc
    webrtc/common_audio/resampler/resampler.cc
    webrtc/common_audio/resampler/sinc_resampler.cc
    webrtc/common_audio/resampler/sinusoidal_linear_chirp_source.cc
    webrtc/common_audio/ring_buffer.c
    webrtc/common_audio/signal_processing/auto_correlation.c
    webrtc/common_audio/signal_processing/auto_corr_to_refl_coef.c
    webrtc/common_audio/signal_processing/complex_bit_reverse.c
    webrtc/common_audio/signal_processing/complex_fft.c
    webrtc/common_audio/signal_processing/copy_set_operations.c
    webrtc/common_audio/signal_processing/cross_correlation.c
    webrtc/common_audio/signal_processing/division_operations.c
    webrtc/common_audio/signal_processing/dot_product_with_scale.cc
    webrtc/common_audio/signal_processing/downsample_fast.c
    webrtc/common_audio/signal_processing/filter_ar.c
    webrtc/common_audio/signal_processing/filter_ar_fast_q12.c
    webrtc/common_audio/signal_processing/energy.c
    webrtc/common_audio/signal_processing/filter_ma_fast_q12.c
    webrtc/common_audio/signal_processing/get_hanning_window.c
    webrtc/common_audio/signal_processing/get_scaling_square.c
    webrtc/common_audio/signal_processing/ilbc_specific_functions.c
    webrtc/common_audio/signal_processing/levinson_durbin.c
    webrtc/common_audio/signal_processing/lpc_to_refl_coef.c
    webrtc/common_audio/signal_processing/min_max_operations.c
    webrtc/common_audio/signal_processing/randomization_functions.c
    webrtc/common_audio/signal_processing/real_fft.c
    webrtc/common_audio/signal_processing/refl_coef_to_lpc.c
    webrtc/common_audio/signal_processing/resample_48khz.c
    webrtc/common_audio/signal_processing/resample_by_2.c
    webrtc/common_audio/signal_processing/resample_by_2_internal.c
    webrtc/common_audio/signal_processing/resample.c
    webrtc/common_audio/signal_processing/resample_fractional.c
    webrtc/common_audio/signal_processing/spl_init.c
    webrtc/common_audio/signal_processing/spl_inl.c
    webrtc/common_audio/signal_processing/splitting_filter.c
    webrtc/common_audio/signal_processing/spl_sqrt.c
    webrtc/common_audio/signal_processing/sqrt_of_one_minus_x_squared.c
    webrtc/common_audio/signal_processing/vector_scaling_operations.c
    webrtc/common_audio/smoothing_filter.cc
    webrtc/common_audio/third_party/ooura/fft_size_128/ooura_fft.cc
    webrtc/common_audio/third_party/ooura/fft_size_256/fft4g.cc
    webrtc/common_audio/third_party/spl_sqrt_floor/spl_sqrt_floor.c
    webrtc/common_audio/vad/vad.cc
    webrtc/common_audio/vad/vad_core.c
    webrtc/common_audio/vad/vad_filterbank.c
    webrtc/common_audio/vad/vad_gmm.c
    webrtc/common_audio/vad/vad_sp.c
    webrtc/common_audio/vad/webrtc_vad.c
    webrtc/common_audio/wav_file.cc
    webrtc/common_audio/wav_header.cc
    webrtc/common_audio/window_generator.cc
)

set(COMMON_AUDIO_SSE2_SOURCES
    webrtc/common_audio/fir_filter_sse.cc
    webrtc/common_audio/resampler/sinc_resampler_sse.cc
    webrtc/common_audio/third_party/ooura/fft_size_128/ooura_fft_sse2.cc
)

set(COMMON_AUDIO_AVX2_SOURCES
    webrtc/common_audio/fir_filter_avx2.cc
    webrtc/common_audio/resampler/sinc_resampler_avx2.cc
)

set(COMMON_AUDIO_NEON_SOURCES
    webrtc/common_audio/fir_filter_neon.cc
    webrtc/common_audio/resampler/sinc_resampler_neon.cc
    webrtc/common_audio/signal_processing/cross_correlation_neon.c
    webrtc/common_audio/signal_processing/downsample_fast_neon.c
    webrtc/common_audio/signal_processing/min_max_operations_neon.c
    webrtc/common_audio/third_party/ooura/fft_size_128/ooura_fft_neon.cc
)

# --- third_party ---
set(FFT_SOURCES webrtc/modules/third_party/fft/fft.c)
set(PFFFT_SOURCES webrtc/third_party/pffft/src/pffft.c)
set(RNNOISE_SOURCES webrtc/third_party/rnnoise/src/rnn_vad_weights.cc)

# --- iSAC VAD (compiled into audio_processing) ---
set(ISAC_VAD_SOURCES
    webrtc/modules/audio_coding/codecs/isac/main/source/filter_functions.c
    webrtc/modules/audio_coding/codecs/isac/main/source/isac_vad.c
    webrtc/modules/audio_coding/codecs/isac/main/source/pitch_estimator.c
    webrtc/modules/audio_coding/codecs/isac/main/source/pitch_filter.c
)

# --- audio_processing (the main module) ---
set(AUDIO_PROCESSING_SOURCES
    webrtc/modules/audio_processing/aec_dump/null_aec_dump_factory.cc
    webrtc/modules/audio_processing/aec3/adaptive_fir_filter.cc
    webrtc/modules/audio_processing/aec3/adaptive_fir_filter_erl.cc
    webrtc/modules/audio_processing/aec3/aec3_common.cc
    webrtc/modules/audio_processing/aec3/aec3_fft.cc
    webrtc/modules/audio_processing/aec3/aec_state.cc
    webrtc/modules/audio_processing/aec3/alignment_mixer.cc
    webrtc/modules/audio_processing/aec3/api_call_jitter_metrics.cc
    webrtc/modules/audio_processing/aec3/block_buffer.cc
    webrtc/modules/audio_processing/aec3/block_delay_buffer.cc
    webrtc/modules/audio_processing/aec3/block_framer.cc
    webrtc/modules/audio_processing/aec3/block_processor.cc
    webrtc/modules/audio_processing/aec3/block_processor_metrics.cc
    webrtc/modules/audio_processing/aec3/clockdrift_detector.cc
    webrtc/modules/audio_processing/aec3/coarse_filter_update_gain.cc
    webrtc/modules/audio_processing/aec3/comfort_noise_generator.cc
    webrtc/modules/audio_processing/aec3/decimator.cc
    webrtc/modules/audio_processing/aec3/dominant_nearend_detector.cc
    webrtc/modules/audio_processing/aec3/downsampled_render_buffer.cc
    webrtc/modules/audio_processing/aec3/echo_audibility.cc
    webrtc/modules/audio_processing/aec3/echo_canceller3.cc
    webrtc/modules/audio_processing/aec3/echo_path_delay_estimator.cc
    webrtc/modules/audio_processing/aec3/echo_path_variability.cc
    webrtc/modules/audio_processing/aec3/echo_remover.cc
    webrtc/modules/audio_processing/aec3/echo_remover_metrics.cc
    webrtc/modules/audio_processing/aec3/erle_estimator.cc
    webrtc/modules/audio_processing/aec3/erl_estimator.cc
    webrtc/modules/audio_processing/aec3/fft_buffer.cc
    webrtc/modules/audio_processing/aec3/filter_analyzer.cc
    webrtc/modules/audio_processing/aec3/frame_blocker.cc
    webrtc/modules/audio_processing/aec3/fullband_erle_estimator.cc
    webrtc/modules/audio_processing/aec3/matched_filter.cc
    webrtc/modules/audio_processing/aec3/matched_filter_lag_aggregator.cc
    webrtc/modules/audio_processing/aec3/moving_average.cc
    webrtc/modules/audio_processing/aec3/refined_filter_update_gain.cc
    webrtc/modules/audio_processing/aec3/render_buffer.cc
    webrtc/modules/audio_processing/aec3/render_delay_buffer.cc
    webrtc/modules/audio_processing/aec3/render_delay_controller.cc
    webrtc/modules/audio_processing/aec3/render_delay_controller_metrics.cc
    webrtc/modules/audio_processing/aec3/render_signal_analyzer.cc
    webrtc/modules/audio_processing/aec3/residual_echo_estimator.cc
    webrtc/modules/audio_processing/aec3/reverb_decay_estimator.cc
    webrtc/modules/audio_processing/aec3/reverb_frequency_response.cc
    webrtc/modules/audio_processing/aec3/reverb_model.cc
    webrtc/modules/audio_processing/aec3/reverb_model_estimator.cc
    webrtc/modules/audio_processing/aec3/signal_dependent_erle_estimator.cc
    webrtc/modules/audio_processing/aec3/spectrum_buffer.cc
    webrtc/modules/audio_processing/aec3/stationarity_estimator.cc
    webrtc/modules/audio_processing/aec3/subband_erle_estimator.cc
    webrtc/modules/audio_processing/aec3/subband_nearend_detector.cc
    webrtc/modules/audio_processing/aec3/subtractor.cc
    webrtc/modules/audio_processing/aec3/subtractor_output_analyzer.cc
    webrtc/modules/audio_processing/aec3/subtractor_output.cc
    webrtc/modules/audio_processing/aec3/suppression_filter.cc
    webrtc/modules/audio_processing/aec3/suppression_gain.cc
    webrtc/modules/audio_processing/aec3/transparent_mode.cc
    webrtc/modules/audio_processing/aecm/aecm_core.cc
    webrtc/modules/audio_processing/aecm/aecm_core_c.cc
    webrtc/modules/audio_processing/aecm/echo_control_mobile.cc
    webrtc/modules/audio_processing/agc/agc.cc
    webrtc/modules/audio_processing/agc/agc_manager_direct.cc
    webrtc/modules/audio_processing/agc/legacy/analog_agc.cc
    webrtc/modules/audio_processing/agc/legacy/digital_agc.cc
    webrtc/modules/audio_processing/agc/loudness_histogram.cc
    webrtc/modules/audio_processing/agc/utility.cc
    webrtc/modules/audio_processing/agc2/adaptive_agc.cc
    webrtc/modules/audio_processing/agc2/adaptive_digital_gain_applier.cc
    webrtc/modules/audio_processing/agc2/adaptive_mode_level_estimator_agc.cc
    webrtc/modules/audio_processing/agc2/adaptive_mode_level_estimator.cc
    webrtc/modules/audio_processing/agc2/agc2_testing_common.cc
    webrtc/modules/audio_processing/agc2/biquad_filter.cc
    webrtc/modules/audio_processing/agc2/compute_interpolated_gain_curve.cc
    webrtc/modules/audio_processing/agc2/down_sampler.cc
    webrtc/modules/audio_processing/agc2/fixed_digital_level_estimator.cc
    webrtc/modules/audio_processing/agc2/gain_applier.cc
    webrtc/modules/audio_processing/agc2/interpolated_gain_curve.cc
    webrtc/modules/audio_processing/agc2/limiter.cc
    webrtc/modules/audio_processing/agc2/limiter_db_gain_curve.cc
    webrtc/modules/audio_processing/agc2/noise_level_estimator.cc
    webrtc/modules/audio_processing/agc2/noise_spectrum_estimator.cc
    webrtc/modules/audio_processing/agc2/rnn_vad/auto_correlation.cc
    webrtc/modules/audio_processing/agc2/rnn_vad/common.cc
    webrtc/modules/audio_processing/agc2/rnn_vad/features_extraction.cc
    webrtc/modules/audio_processing/agc2/rnn_vad/lp_residual.cc
    webrtc/modules/audio_processing/agc2/rnn_vad/pitch_search.cc
    webrtc/modules/audio_processing/agc2/rnn_vad/pitch_search_internal.cc
    webrtc/modules/audio_processing/agc2/rnn_vad/rnn.cc
    webrtc/modules/audio_processing/agc2/rnn_vad/spectral_features.cc
    webrtc/modules/audio_processing/agc2/rnn_vad/spectral_features_internal.cc
    webrtc/modules/audio_processing/agc2/saturation_protector.cc
    webrtc/modules/audio_processing/agc2/signal_classifier.cc
    webrtc/modules/audio_processing/agc2/vad_with_level.cc
    webrtc/modules/audio_processing/agc2/vector_float_frame.cc
    webrtc/modules/audio_processing/audio_buffer.cc
    webrtc/modules/audio_processing/audio_processing_builder_impl.cc
    webrtc/modules/audio_processing/audio_processing_impl.cc
    webrtc/modules/audio_processing/echo_control_mobile_impl.cc
    webrtc/modules/audio_processing/echo_detector/circular_buffer.cc
    webrtc/modules/audio_processing/echo_detector/mean_variance_estimator.cc
    webrtc/modules/audio_processing/echo_detector/moving_max.cc
    webrtc/modules/audio_processing/echo_detector/normalized_covariance_estimator.cc
    webrtc/modules/audio_processing/gain_control_impl.cc
    webrtc/modules/audio_processing/gain_controller2.cc
    webrtc/modules/audio_processing/high_pass_filter.cc
    webrtc/modules/audio_processing/include/aec_dump.cc
    webrtc/modules/audio_processing/include/audio_frame_proxies.cc
    webrtc/modules/audio_processing/include/audio_processing.cc
    webrtc/modules/audio_processing/include/audio_processing_statistics.cc
    webrtc/modules/audio_processing/include/config.cc
    webrtc/modules/audio_processing/level_estimator.cc
    webrtc/modules/audio_processing/logging/apm_data_dumper.cc
    webrtc/modules/audio_processing/ns/fast_math.cc
    webrtc/modules/audio_processing/ns/histograms.cc
    webrtc/modules/audio_processing/ns/noise_estimator.cc
    webrtc/modules/audio_processing/ns/noise_suppressor.cc
    webrtc/modules/audio_processing/ns/ns_fft.cc
    webrtc/modules/audio_processing/ns/prior_signal_model.cc
    webrtc/modules/audio_processing/ns/prior_signal_model_estimator.cc
    webrtc/modules/audio_processing/ns/quantile_noise_estimator.cc
    webrtc/modules/audio_processing/ns/signal_model.cc
    webrtc/modules/audio_processing/ns/signal_model_estimator.cc
    webrtc/modules/audio_processing/ns/speech_probability_estimator.cc
    webrtc/modules/audio_processing/ns/suppression_params.cc
    webrtc/modules/audio_processing/ns/wiener_filter.cc
    webrtc/modules/audio_processing/optionally_built_submodule_creators.cc
    webrtc/modules/audio_processing/residual_echo_detector.cc
    webrtc/modules/audio_processing/rms_level.cc
    webrtc/modules/audio_processing/splitting_filter.cc
    webrtc/modules/audio_processing/three_band_filter_bank.cc
    webrtc/modules/audio_processing/transient/file_utils.cc
    webrtc/modules/audio_processing/transient/moving_moments.cc
    webrtc/modules/audio_processing/transient/transient_detector.cc
    webrtc/modules/audio_processing/transient/transient_suppressor_impl.cc
    webrtc/modules/audio_processing/transient/wpd_node.cc
    webrtc/modules/audio_processing/transient/wpd_tree.cc
    webrtc/modules/audio_processing/typing_detection.cc
    webrtc/modules/audio_processing/utility/cascaded_biquad_filter.cc
    webrtc/modules/audio_processing/utility/delay_estimator.cc
    webrtc/modules/audio_processing/utility/delay_estimator_wrapper.cc
    webrtc/modules/audio_processing/utility/pffft_wrapper.cc
    webrtc/modules/audio_processing/vad/gmm.cc
    webrtc/modules/audio_processing/vad/pitch_based_vad.cc
    webrtc/modules/audio_processing/vad/pitch_internal.cc
    webrtc/modules/audio_processing/vad/pole_zero_filter.cc
    webrtc/modules/audio_processing/vad/standalone_vad.cc
    webrtc/modules/audio_processing/vad/vad_audio_proc.cc
    webrtc/modules/audio_processing/vad/vad_circular_buffer.cc
    webrtc/modules/audio_processing/vad/voice_activity_detector.cc
    webrtc/modules/audio_processing/voice_detection.cc
)

set(AUDIO_PROCESSING_AVX2_SOURCES
    webrtc/modules/audio_processing/aec3/adaptive_fir_filter_avx2.cc
    webrtc/modules/audio_processing/aec3/adaptive_fir_filter_erl_avx2.cc
    webrtc/modules/audio_processing/aec3/fft_data_avx2.cc
    webrtc/modules/audio_processing/aec3/matched_filter_avx2.cc
    webrtc/modules/audio_processing/aec3/vector_math_avx2.cc
)

set(AUDIO_PROCESSING_NEON_SOURCES
    webrtc/modules/audio_processing/aecm/aecm_core_neon.cc
)

# ============================================================================
# Build targets
# ============================================================================

# Collect all base sources into one static library
set(ALL_SOURCES
    ${RTC_BASE_SOURCES}
    ${API_SOURCES}
    ${SYSTEM_WRAPPERS_SOURCES}
    ${COMMON_AUDIO_SOURCES}
    ${FFT_SOURCES}
    ${PFFFT_SOURCES}
    ${RNNOISE_SOURCES}
    ${ISAC_VAD_SOURCES}
    ${AUDIO_PROCESSING_SOURCES}
)

# Prepend base path to all sources
list(TRANSFORM ALL_SOURCES PREPEND "${WAP_ROOT}/")

add_library(webrtc_audio_processing STATIC ${ALL_SOURCES})

target_compile_definitions(webrtc_audio_processing PRIVATE ${WAP_DEFINES})

# pffft needs _GNU_SOURCE
set_source_files_properties("${WAP_ROOT}/webrtc/third_party/pffft/src/pffft.c"
    PROPERTIES COMPILE_DEFINITIONS "_GNU_SOURCE"
)

target_include_directories(webrtc_audio_processing
    PUBLIC  ${WAP_ROOT} ${WAP_ROOT}/webrtc
)

target_link_libraries(webrtc_audio_processing
    PUBLIC
        absl::base
        absl::flags
        absl::strings
        absl::synchronization
        absl::bad_optional_access
    PRIVATE
        ${WAP_LIBS}
)

# --- x86 SIMD: SSE2 and AVX2 object libraries ---
if(WAP_HAS_X86)
    set(SSE2_SOURCES ${COMMON_AUDIO_SSE2_SOURCES})
    list(TRANSFORM SSE2_SOURCES PREPEND "${WAP_ROOT}/")
    add_library(wap_sse2 OBJECT ${SSE2_SOURCES})
    target_compile_definitions(wap_sse2 PRIVATE ${WAP_DEFINES})
    target_include_directories(wap_sse2 PRIVATE ${WAP_ROOT} ${WAP_ROOT}/webrtc)
    target_link_libraries(wap_sse2 PRIVATE absl::base)
    if(MSVC)
        target_compile_options(wap_sse2 PRIVATE /wd4244 /wd4267 /wd4305 /wd4100)
    else()
        target_compile_options(wap_sse2 PRIVATE -msse2 -Wno-unused-parameter)
    endif()

    set(AVX2_SOURCES ${COMMON_AUDIO_AVX2_SOURCES} ${AUDIO_PROCESSING_AVX2_SOURCES})
    list(TRANSFORM AVX2_SOURCES PREPEND "${WAP_ROOT}/")
    add_library(wap_avx2 OBJECT ${AVX2_SOURCES})
    target_compile_definitions(wap_avx2 PRIVATE ${WAP_DEFINES})
    target_include_directories(wap_avx2 PRIVATE ${WAP_ROOT} ${WAP_ROOT}/webrtc)
    target_link_libraries(wap_avx2 PRIVATE absl::base)
    if(MSVC)
        target_compile_options(wap_avx2 PRIVATE /arch:AVX2 /wd4244 /wd4267 /wd4305 /wd4100)
    else()
        target_compile_options(wap_avx2 PRIVATE -mavx2 -mfma -Wno-unused-parameter)
    endif()

    target_sources(webrtc_audio_processing PRIVATE
        $<TARGET_OBJECTS:wap_sse2>
        $<TARGET_OBJECTS:wap_avx2>
    )
endif()

# --- ARM NEON object library ---
if(WAP_HAS_NEON)
    set(NEON_SOURCES ${COMMON_AUDIO_NEON_SOURCES} ${AUDIO_PROCESSING_NEON_SOURCES})
    list(TRANSFORM NEON_SOURCES PREPEND "${WAP_ROOT}/")
    add_library(wap_neon OBJECT ${NEON_SOURCES})
    target_compile_definitions(wap_neon PRIVATE ${WAP_DEFINES})
    target_include_directories(wap_neon PRIVATE ${WAP_ROOT} ${WAP_ROOT}/webrtc)
    target_link_libraries(wap_neon PRIVATE absl::base)

    target_sources(webrtc_audio_processing PRIVATE
        $<TARGET_OBJECTS:wap_neon>
    )
endif()

# Suppress warnings in vendored code
if(MSVC)
    target_compile_options(webrtc_audio_processing PRIVATE
        /wd4244 /wd4267 /wd4305 /wd4838 /wd4065 /wd4702 /wd4018
        /wd4334 /wd4996 /wd4100 /wd4127 /wd4189 /wd4456
    )
else()
    target_compile_options(webrtc_audio_processing PRIVATE
        -Wno-unused-parameter -Wno-sign-compare -Wno-missing-field-initializers
    )
endif()
