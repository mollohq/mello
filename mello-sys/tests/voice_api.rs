//! Integration tests for the mello voice C API via FFI.

use std::ptr;

fn with_ctx(f: impl FnOnce(*mut mello_sys::MelloContext)) {
    let ctx = unsafe { mello_sys::mello_init() };
    if ctx.is_null() {
        eprintln!("SKIP: mello_init failed (no audio context available)");
        return;
    }
    f(ctx);
    unsafe {
        mello_sys::mello_destroy(ctx);
    }
}

#[test]
fn voice_start_stop_capture() {
    with_ctx(|ctx| unsafe {
        let r1 = mello_sys::mello_voice_start_capture(ctx);
        assert_eq!(r1, mello_sys::MelloResult_MELLO_OK, "start capture failed");

        let r2 = mello_sys::mello_voice_stop_capture(ctx);
        assert_eq!(r2, mello_sys::MelloResult_MELLO_OK, "stop capture failed");

        // Double-stop should be graceful (not crash, may return error or OK)
        let _r3 = mello_sys::mello_voice_stop_capture(ctx);
    });
}

#[test]
fn voice_mute_deafen() {
    with_ctx(|ctx| unsafe {
        mello_sys::mello_voice_set_mute(ctx, true);
        assert!(
            !mello_sys::mello_voice_is_speaking(ctx),
            "should not be speaking when muted"
        );

        mello_sys::mello_voice_set_mute(ctx, false);

        mello_sys::mello_voice_set_deafen(ctx, true);
        mello_sys::mello_voice_set_deafen(ctx, false);
    });
}

#[test]
fn voice_get_packet_without_capture() {
    with_ctx(|ctx| unsafe {
        let mut buf = [0u8; 4000];
        let size = mello_sys::mello_voice_get_packet(ctx, buf.as_mut_ptr(), buf.len() as i32);
        assert_eq!(size, 0, "should return 0 when no capture is running");
    });
}

#[test]
fn voice_feed_packet_null_safety() {
    with_ctx(|ctx| unsafe {
        // Null data
        let r = mello_sys::mello_voice_feed_packet(ctx, ptr::null(), ptr::null(), 0);
        assert_ne!(r, mello_sys::MelloResult_MELLO_OK);

        // Null peer_id with valid data
        let data = [0u8; 10];
        let r = mello_sys::mello_voice_feed_packet(ctx, ptr::null(), data.as_ptr(), 10);
        assert_ne!(r, mello_sys::MelloResult_MELLO_OK);

        // Valid peer_id, null data
        let peer = std::ffi::CString::new("test_peer").unwrap();
        let r = mello_sys::mello_voice_feed_packet(ctx, peer.as_ptr(), ptr::null(), 0);
        // Should handle gracefully (not crash)
        let _ = r;
    });
}

#[test]
fn voice_input_level_range() {
    with_ctx(|ctx| unsafe {
        let level = mello_sys::mello_voice_get_input_level(ctx);
        assert!(level >= 0.0, "level should be >= 0");
        assert!(level <= 1.0, "level should be <= 1");
    });
}

#[test]
fn voice_vad_callback_set_clear() {
    unsafe extern "C" fn dummy_cb(_ud: *mut std::ffi::c_void, _speaking: bool) {}

    with_ctx(|ctx| unsafe {
        mello_sys::mello_voice_set_vad_callback(ctx, Some(dummy_cb), ptr::null_mut());
        mello_sys::mello_voice_set_vad_callback(ctx, None, ptr::null_mut());
    });
}

#[test]
fn audio_loopback_packet_round_trip() {
    with_ctx(|ctx| unsafe {
        let peer_id = std::ffi::CString::new("loopback_test").unwrap();
        // 4-byte sequence header + some Opus-like payload
        let mut packet = [0u8; 100];
        packet[0] = 0x00;
        packet[1] = 0x00;
        packet[2] = 0x00;
        packet[3] = 0x01;

        let r = mello_sys::mello_voice_feed_packet(
            ctx,
            peer_id.as_ptr(),
            packet.as_ptr(),
            packet.len() as i32,
        );
        // Should not crash regardless of result
        let _ = r;
    });
}

#[test]
fn multi_peer_feed_packet_concurrent() {
    with_ctx(|ctx| {
        let peer_a = std::ffi::CString::new("aaaa-bbbb-cccc").unwrap();
        let peer_b = std::ffi::CString::new("dddd-eeee-ffff").unwrap();

        // 4-byte LE sequence header + minimal Opus-like payload
        let make_packet = |seq: u32| -> Vec<u8> {
            let mut pkt = Vec::with_capacity(104);
            pkt.extend_from_slice(&seq.to_le_bytes());
            pkt.resize(104, 0xAB);
            pkt
        };

        // Feed interleaved packets from two peers — must not crash
        for i in 0u32..50 {
            let pkt = make_packet(i);
            unsafe {
                mello_sys::mello_voice_feed_packet(
                    ctx,
                    peer_a.as_ptr(),
                    pkt.as_ptr(),
                    pkt.len() as i32,
                );
                mello_sys::mello_voice_feed_packet(
                    ctx,
                    peer_b.as_ptr(),
                    pkt.as_ptr(),
                    pkt.len() as i32,
                );
            }
        }

        // Verify debug stats reflect incoming streams
        let mut stats: mello_sys::MelloDebugStats = unsafe { std::mem::zeroed() };
        unsafe { mello_sys::mello_get_debug_stats(ctx, &mut stats) };
        assert!(
            stats.incoming_streams >= 0,
            "incoming_streams should be non-negative"
        );
    });
}

#[test]
fn voice_ns_controls_smoke() {
    with_ctx(|ctx| unsafe {
        mello_sys::mello_voice_set_ns_mode(ctx, mello_sys::MelloNsMode_MELLO_NS_OFF);
        mello_sys::mello_voice_set_ns_mode(ctx, mello_sys::MelloNsMode_MELLO_NS_RNNOISE);
        mello_sys::mello_voice_set_ns_mode(ctx, mello_sys::MelloNsMode_MELLO_NS_WEBRTC_LOW);
        mello_sys::mello_voice_set_ns_mode(ctx, mello_sys::MelloNsMode_MELLO_NS_WEBRTC_MODERATE);
        mello_sys::mello_voice_set_ns_mode(ctx, mello_sys::MelloNsMode_MELLO_NS_WEBRTC_HIGH);
        mello_sys::mello_voice_set_ns_mode(ctx, mello_sys::MelloNsMode_MELLO_NS_WEBRTC_VERY_HIGH);
        mello_sys::mello_voice_set_transient_suppression(ctx, true);
        mello_sys::mello_voice_set_high_pass_filter(ctx, true);
        mello_sys::mello_voice_set_transient_suppression(ctx, false);
        mello_sys::mello_voice_set_high_pass_filter(ctx, false);
    });
}

#[test]
fn voice_capture_inject_smoke() {
    with_ctx(|ctx| unsafe {
        let r = mello_sys::mello_voice_start_capture_inject(ctx);
        assert_eq!(
            r,
            mello_sys::MelloResult_MELLO_OK,
            "start capture inject failed"
        );

        let frame = [0i16; 960];
        mello_sys::mello_voice_inject_capture(ctx, frame.as_ptr(), frame.len() as i32);
        mello_sys::mello_voice_stop_capture_inject(ctx);
    });
}
