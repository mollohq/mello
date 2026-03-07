//! FFI bindings to libmello

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

// Include generated bindings
// include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

// Placeholder types until we have real bindings
pub type MelloContext = std::ffi::c_void;

// Placeholder functions
pub unsafe fn mello_init() -> *mut MelloContext {
    std::ptr::null_mut()
}

pub unsafe fn mello_destroy(_ctx: *mut MelloContext) {}
