// The gate binary finds libonnxruntime next to itself in target/release/
// (mello-sys copies it there), as the client does (client/build.rs).
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path");
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
}
