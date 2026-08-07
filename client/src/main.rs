// Thin binary wrapper. All logic lives in the `mello_client` library so that
// integration tests and the headless UI harness can drive the same wiring the
// shipped app uses. See client/src/lib.rs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    mello_client::run();
}
