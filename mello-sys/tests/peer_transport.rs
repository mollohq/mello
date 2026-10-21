//! Integration tests of the peer and context C API that need no audio
//! hardware and no network.

use std::ffi::CString;

#[test]
fn context_init_destroy() {
    let ctx = unsafe { mello_sys::mello_init() };
    if ctx.is_null() {
        eprintln!("SKIP: mello_init failed (no audio context available)");
        return;
    }
    unsafe {
        mello_sys::mello_destroy(ctx);
    }
}

#[test]
fn peer_null_safety() {
    assert!(unsafe { mello_sys::mello_peer_create_offer(std::ptr::null_mut()) }.is_null());
    assert!(
        unsafe { mello_sys::mello_peer_create_answer(std::ptr::null_mut(), std::ptr::null()) }
            .is_null()
    );
    assert!(!unsafe { mello_sys::mello_peer_is_connected(std::ptr::null_mut()) });
}

#[test]
fn peer_create_destroy_many() {
    let ctx = unsafe { mello_sys::mello_init() };
    if ctx.is_null() {
        eprintln!("SKIP: mello_init failed (no audio context available)");
        return;
    }

    let mut peers = Vec::new();
    for i in 0..5 {
        let id = CString::new(format!("peer_{}", i)).unwrap();
        let peer = unsafe { mello_sys::mello_peer_create(ctx, id.as_ptr()) };
        assert!(!peer.is_null(), "peer {} create failed", i);
        peers.push(peer);
    }

    for peer in peers {
        unsafe {
            mello_sys::mello_peer_destroy(peer);
        }
    }
    unsafe {
        mello_sys::mello_destroy(ctx);
    }
}

#[test]
fn voice_null_context_safety() {
    let null_ctx: *mut mello_sys::MelloContext = std::ptr::null_mut();
    unsafe {
        let _ = mello_sys::mello_voice_start_capture(null_ctx);
        let _ = mello_sys::mello_voice_stop_capture(null_ctx);
        mello_sys::mello_voice_set_mute(null_ctx, true);
        mello_sys::mello_voice_set_deafen(null_ctx, true);
        assert!(!mello_sys::mello_voice_is_speaking(null_ctx));
        assert_eq!(mello_sys::mello_voice_get_input_level(null_ctx), 0.0);

        let mut buf = [0u8; 100];
        assert_eq!(
            mello_sys::mello_voice_get_packet(null_ctx, buf.as_mut_ptr(), 100),
            0
        );

        let peer_id = CString::new("test").unwrap();
        let _ = mello_sys::mello_voice_feed_packet(null_ctx, peer_id.as_ptr(), buf.as_ptr(), 10);
    }
}
