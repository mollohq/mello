/*
 * iOS link stub for libmello.
 *
 * The real libmello has no iOS build yet (see mello-ios/specs/IOS-LIBMELLO-PORT.md).
 * To unblock the FFI spine on iOS, mello-sys/build.rs compiles this stub instead of
 * building libmello when CARGO_CFG_TARGET_OS == "ios". It defines every function in
 * mello.h as an inert no-op so the iOS app links, while bindgen still generates the
 * real Rust types from the same header (so mello-core compiles unchanged).
 *
 * mello_init() returns NULL on purpose: mello-core's voice/stream code already guards
 * every call with `if ctx.is_null() { return; }`, so audio/video become no-ops and the
 * Nakama paths (auth, crews, chat) run normally. When the real libmello-iOS port lands,
 * the build.rs iOS branch switches back to building libmello and this file is dropped.
 *
 * This file #includes mello.h, so the C compiler enforces that every definition matches
 * the header declaration -- there is no signature drift risk.
 */

#include "mello.h"
#include <stddef.h>

/* ---- Context ---- */
MelloContext* mello_init(void) { return NULL; }
void mello_destroy(MelloContext* ctx) { (void)ctx; }
const char* mello_get_error(MelloContext* ctx) { (void)ctx; return NULL; }
void mello_set_log_callback(MelloLogCallback callback, void* user_data) { (void)callback; (void)user_data; }

/* ---- Microphone permission ---- */
MelloMicPermission mello_mic_permission_status(void) { return MELLO_MIC_NOT_DETERMINED; }
void mello_mic_request_permission(MelloMicPermissionCallback callback, void* user_data) { (void)callback; (void)user_data; }

/* ---- Voice ---- */
MelloResult mello_voice_start_capture(MelloContext* ctx) { (void)ctx; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_voice_stop_capture(MelloContext* ctx) { (void)ctx; return MELLO_ERROR_NOT_INITIALIZED; }
void mello_voice_set_mute(MelloContext* ctx, bool muted) { (void)ctx; (void)muted; }
void mello_voice_set_deafen(MelloContext* ctx, bool deafened) { (void)ctx; (void)deafened; }
void mello_voice_set_push_to_talk(MelloContext* ctx, bool enabled) { (void)ctx; (void)enabled; }
bool mello_voice_is_speaking(MelloContext* ctx) { (void)ctx; return false; }
void mello_voice_set_vad_callback(MelloContext* ctx, MelloVoiceActivityCallback callback, void* user_data) { (void)ctx; (void)callback; (void)user_data; }
void mello_voice_set_echo_cancellation(MelloContext* ctx, bool enabled) { (void)ctx; (void)enabled; }
void mello_voice_set_agc(MelloContext* ctx, bool enabled) { (void)ctx; (void)enabled; }
void mello_voice_set_noise_suppression(MelloContext* ctx, bool enabled) { (void)ctx; (void)enabled; }
void mello_voice_set_ns_mode(MelloContext* ctx, MelloNsMode mode) { (void)ctx; (void)mode; }
void mello_voice_set_transient_suppression(MelloContext* ctx, bool enabled) { (void)ctx; (void)enabled; }
void mello_voice_set_high_pass_filter(MelloContext* ctx, bool enabled) { (void)ctx; (void)enabled; }
void mello_voice_set_input_volume(MelloContext* ctx, float volume) { (void)ctx; (void)volume; }
void mello_voice_set_output_volume(MelloContext* ctx, float volume) { (void)ctx; (void)volume; }
float mello_voice_get_input_level(MelloContext* ctx) { (void)ctx; return 0.0f; }
int mello_voice_get_packet(MelloContext* ctx, uint8_t* buffer, int buffer_size) { (void)ctx; (void)buffer; (void)buffer_size; return 0; }
MelloResult mello_voice_feed_packet(MelloContext* ctx, const char* peer_id, const uint8_t* data, int size) { (void)ctx; (void)peer_id; (void)data; (void)size; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_voice_start_capture_inject(MelloContext* ctx) { (void)ctx; return MELLO_ERROR_NOT_INITIALIZED; }
void mello_voice_inject_capture(MelloContext* ctx, const int16_t* samples, int count) { (void)ctx; (void)samples; (void)count; }
void mello_voice_stop_capture_inject(MelloContext* ctx) { (void)ctx; }

/* ---- Clip buffer ---- */
MelloResult mello_clip_buffer_start(MelloContext* ctx) { (void)ctx; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_clip_buffer_stop(MelloContext* ctx) { (void)ctx; return MELLO_ERROR_NOT_INITIALIZED; }
bool mello_clip_buffer_active(MelloContext* ctx) { (void)ctx; return false; }
MelloResult mello_clip_capture(MelloContext* ctx, float seconds, const char* output_path) { (void)ctx; (void)seconds; (void)output_path; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_clip_play(MelloContext* ctx, const char* wav_path) { (void)ctx; (void)wav_path; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_clip_play_mp4(MelloContext* ctx, const char* mp4_path) { (void)ctx; (void)mp4_path; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_clip_stop_playback(MelloContext* ctx) { (void)ctx; return MELLO_ERROR_NOT_INITIALIZED; }
bool mello_clip_is_playing(MelloContext* ctx) { (void)ctx; return false; }
void mello_clip_playback_progress(MelloContext* ctx, uint64_t* position_samples, uint64_t* total_samples, uint32_t* sample_rate) { (void)ctx; (void)position_samples; (void)total_samples; (void)sample_rate; }
MelloResult mello_clip_pause(MelloContext* ctx) { (void)ctx; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_clip_resume(MelloContext* ctx) { (void)ctx; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_clip_seek(MelloContext* ctx, uint64_t position_samples) { (void)ctx; (void)position_samples; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_clip_encode(const char* wav_path, const char* mp4_path, int bitrate) { (void)wav_path; (void)mp4_path; (void)bitrate; return MELLO_ERROR_NOT_INITIALIZED; }

/* ---- P2P transport ---- */
MelloPeerConnection* mello_peer_create(MelloContext* ctx, const char* peer_id) { (void)ctx; (void)peer_id; return NULL; }
void mello_peer_destroy(MelloPeerConnection* peer) { (void)peer; }
void mello_peer_set_ice_servers(MelloPeerConnection* peer, const char** urls, int count) { (void)peer; (void)urls; (void)count; }
const char* mello_peer_create_offer(MelloPeerConnection* peer) { (void)peer; return NULL; }
const char* mello_peer_create_answer(MelloPeerConnection* peer, const char* offer_sdp) { (void)peer; (void)offer_sdp; return NULL; }
MelloResult mello_peer_set_remote_description(MelloPeerConnection* peer, const char* sdp, bool is_offer) { (void)peer; (void)sdp; (void)is_offer; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_peer_add_ice_candidate(MelloPeerConnection* peer, const MelloIceCandidate* candidate) { (void)peer; (void)candidate; return MELLO_ERROR_NOT_INITIALIZED; }
void mello_peer_set_ice_callback(MelloPeerConnection* peer, MelloIceCandidateCallback callback, void* user_data) { (void)peer; (void)callback; (void)user_data; }
void mello_peer_set_state_callback(MelloPeerConnection* peer, MelloPeerStateCallback callback, void* user_data) { (void)peer; (void)callback; (void)user_data; }
void mello_peer_set_data_callback(MelloPeerConnection* peer, MelloPeerDataCallback callback, void* user_data) { (void)peer; (void)callback; (void)user_data; }
void mello_peer_set_audio_track_callback(MelloPeerConnection* peer, MelloAudioTrackCallback callback, void* user_data) { (void)peer; (void)callback; (void)user_data; }
MelloResult mello_peer_send_unreliable(MelloPeerConnection* peer, const uint8_t* data, int size) { (void)peer; (void)data; (void)size; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_peer_send_reliable(MelloPeerConnection* peer, const uint8_t* data, int size) { (void)peer; (void)data; (void)size; return MELLO_ERROR_NOT_INITIALIZED; }
bool mello_peer_is_connected(MelloPeerConnection* peer) { (void)peer; return false; }
bool mello_peer_is_unreliable_open(MelloPeerConnection* peer) { (void)peer; return false; }
bool mello_peer_is_reliable_open(MelloPeerConnection* peer) { (void)peer; return false; }
MelloResult mello_peer_send_audio(MelloPeerConnection* peer, const uint8_t* data, int size) { (void)peer; (void)data; (void)size; return MELLO_ERROR_NOT_INITIALIZED; }
const char* mello_peer_handle_remote_offer(MelloPeerConnection* peer, const char* offer_sdp) { (void)peer; (void)offer_sdp; return NULL; }
int mello_peer_recv(MelloPeerConnection* peer, uint8_t* buffer, int buffer_size) { (void)peer; (void)buffer; (void)buffer_size; return 0; }
void mello_peer_send_ping(MelloPeerConnection* peer) { (void)peer; }
float mello_peer_rtt_ms(MelloPeerConnection* peer) { (void)peer; return 0.0f; }
int mello_peer_send_audio_skips(MelloPeerConnection* peer) { (void)peer; return 0; }
int mello_peer_recv_track_count(MelloPeerConnection* peer) { (void)peer; return 0; }

/* ---- Video / streaming ---- */
int mello_get_encoders(MelloContext* ctx, MelloEncoderBackend* out, int max_count) { (void)ctx; (void)out; (void)max_count; return 0; }
int mello_get_decoders(MelloContext* ctx, MelloDecoderBackend* out, int max_count) { (void)ctx; (void)out; (void)max_count; return 0; }
bool mello_encoder_available(MelloContext* ctx) { (void)ctx; return false; }
int mello_enumerate_monitors(MelloContext* ctx, MelloMonitorInfo* out, int max_count) { (void)ctx; (void)out; (void)max_count; return 0; }
int mello_enumerate_games(MelloContext* ctx, MelloGameProcess* out, int max_count) { (void)ctx; (void)out; (void)max_count; return 0; }
int mello_enumerate_windows(MelloContext* ctx, MelloWindow* out, int max_count) { (void)ctx; (void)out; (void)max_count; return 0; }
int mello_capture_window_thumbnail(void* hwnd, uint32_t max_width, uint32_t max_height, uint8_t* rgba_out, uint32_t* out_width, uint32_t* out_height) { (void)hwnd; (void)max_width; (void)max_height; (void)rgba_out; (void)out_width; (void)out_height; return -1; }
MelloStreamHost* mello_stream_start_host(MelloContext* ctx, const MelloCaptureSource* source, const MelloStreamConfig* config, MelloPacketCallback on_packet, void* user_data) { (void)ctx; (void)source; (void)config; (void)on_packet; (void)user_data; return NULL; }
void mello_stream_stop_host(MelloStreamHost* host) { (void)host; }
void mello_stream_get_host_resolution(MelloStreamHost* host, uint32_t* width, uint32_t* height) { (void)host; (void)width; (void)height; }
void mello_stream_request_keyframe(MelloStreamHost* host) { (void)host; }
MelloResult mello_stream_set_bitrate(MelloStreamHost* host, uint32_t bitrate_kbps) { (void)host; (void)bitrate_kbps; return MELLO_ERROR_NOT_INITIALIZED; }
void mello_stream_set_audio_callback(MelloStreamHost* host, MelloAudioPacketCallback callback, void* user_data) { (void)host; (void)callback; (void)user_data; }
MelloResult mello_stream_start_audio(MelloStreamHost* host) { (void)host; return MELLO_ERROR_NOT_INITIALIZED; }
void mello_stream_stop_audio(MelloStreamHost* host) { (void)host; }
MelloStreamView* mello_stream_start_viewer(MelloContext* ctx, const MelloStreamConfig* config, MelloFrameCallback on_frame, void* user_data) { (void)ctx; (void)config; (void)on_frame; (void)user_data; return NULL; }
void mello_stream_stop_viewer(MelloStreamView* view) { (void)view; }
bool mello_stream_feed_packet(MelloStreamView* view, const uint8_t* data, int size, bool is_keyframe) { (void)view; (void)data; (void)size; (void)is_keyframe; return false; }
int mello_stream_viewer_decode_queue_depth(MelloStreamView* view) { (void)view; return 0; }
bool mello_stream_present_frame(MelloStreamView* view) { (void)view; return false; }
void mello_stream_set_native_frame_callback(MelloStreamView* view, MelloNativeFrameCallback callback, void* user_data) { (void)view; (void)callback; (void)user_data; }
MelloResult mello_stream_feed_audio_packet(MelloStreamView* view, const uint8_t* data, int size) { (void)view; (void)data; (void)size; return MELLO_ERROR_NOT_INITIALIZED; }
void mello_stream_get_stats(MelloStreamHost* host, MelloStreamStats* stats) { (void)host; (void)stats; }
int mello_stream_get_cursor_packet(MelloStreamHost* host, uint8_t* buf, int buf_size) { (void)host; (void)buf; (void)buf_size; return 0; }
MelloResult mello_stream_apply_cursor_packet(MelloStreamView* view, const uint8_t* buf, int size) { (void)view; (void)buf; (void)size; return MELLO_ERROR_NOT_INITIALIZED; }
void mello_stream_get_cursor_state(MelloStreamView* view, MelloCursorState* out) { (void)view; (void)out; }

/* ---- Debug / diagnostics ---- */
void mello_get_debug_stats(MelloContext* ctx, MelloDebugStats* out) { (void)ctx; (void)out; }

/* ---- Devices ---- */
int mello_get_audio_inputs(MelloContext* ctx, MelloDevice* devices, int max_count) { (void)ctx; (void)devices; (void)max_count; return 0; }
int mello_get_audio_outputs(MelloContext* ctx, MelloDevice* devices, int max_count) { (void)ctx; (void)devices; (void)max_count; return 0; }
void mello_free_device_list(MelloDevice* devices, int count) { (void)devices; (void)count; }
MelloResult mello_set_audio_input(MelloContext* ctx, const char* device_id) { (void)ctx; (void)device_id; return MELLO_ERROR_NOT_INITIALIZED; }
MelloResult mello_set_audio_output(MelloContext* ctx, const char* device_id) { (void)ctx; (void)device_id; return MELLO_ERROR_NOT_INITIALIZED; }
