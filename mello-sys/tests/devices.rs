//! Integration tests for device enumeration and selection via FFI.

use std::ptr;

fn with_ctx(f: impl FnOnce(*mut mello_sys::MelloContext)) {
    let ctx = unsafe { mello_sys::mello_init() };
    assert!(!ctx.is_null(), "mello_init failed");
    f(ctx);
    unsafe {
        mello_sys::mello_destroy(ctx);
    }
}

#[test]
fn enumerate_capture_devices() {
    with_ctx(|ctx| unsafe {
        let mut devices = vec![std::mem::zeroed::<mello_sys::MelloDevice>(); 32];
        let count = mello_sys::mello_get_audio_inputs(ctx, devices.as_mut_ptr(), 32);
        assert!(count >= 0, "count should be non-negative");

        if count > 0 {
            let slice = &devices[..count as usize];
            let has_default = slice.iter().any(|d| d.is_default);
            assert!(has_default, "at least one capture device should be default");
        }

        mello_sys::mello_free_device_list(devices.as_mut_ptr(), count);
    });
}

#[test]
fn enumerate_playback_devices() {
    with_ctx(|ctx| unsafe {
        let mut devices = vec![std::mem::zeroed::<mello_sys::MelloDevice>(); 32];
        let count = mello_sys::mello_get_audio_outputs(ctx, devices.as_mut_ptr(), 32);
        assert!(count >= 0, "count should be non-negative");

        if count > 0 {
            let slice = &devices[..count as usize];
            let has_default = slice.iter().any(|d| d.is_default);
            assert!(
                has_default,
                "at least one playback device should be default"
            );
        }

        mello_sys::mello_free_device_list(devices.as_mut_ptr(), count);
    });
}

#[test]
fn set_audio_input_null() {
    with_ctx(|ctx| unsafe {
        let r = mello_sys::mello_set_audio_input(ctx, ptr::null());
        assert_eq!(
            r,
            mello_sys::MelloResult_MELLO_OK,
            "null should revert to default"
        );
    });
}

#[test]
fn set_audio_output_null() {
    with_ctx(|ctx| unsafe {
        let r = mello_sys::mello_set_audio_output(ctx, ptr::null());
        assert_eq!(
            r,
            mello_sys::MelloResult_MELLO_OK,
            "null should revert to default"
        );
    });
}

#[test]
fn set_audio_input_invalid() {
    with_ctx(|ctx| unsafe {
        let bogus = std::ffi::CString::new("nonexistent_device_id_xyz").unwrap();
        let r = mello_sys::mello_set_audio_input(ctx, bogus.as_ptr());
        // Should not crash. May return OK (WASAPI falls back) or error.
        let _ = r;
    });
}
