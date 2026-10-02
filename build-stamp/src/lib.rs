//! The build stamp: which commit made this binary.
//!
//! `build.rs` records the values at build time. The client prints them in
//! `mello --build-info`. The stamp lives in its own crate. Its
//! `rerun-if-changed` list names the client sources, and in the client's own
//! build script that list would compile the whole Slint UI again after every
//! Rust edit.

include!(concat!(env!("OUT_DIR"), "/stamp.rs"));
