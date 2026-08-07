fn main() {
    // Set @executable_path as rpath so the binary finds libonnxruntime.dylib
    // next to itself (in .app bundle or in target/release/)
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");

    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icons/mello.ico");
        res.compile().expect("failed to compile windows resources");
    }

    let style = if cfg!(target_os = "macos") {
        "cupertino"
    } else {
        "fluent"
    };

    // The test harness locates UI elements via `ElementHandle`, which needs
    // element metadata embedded in the generated code. Without it every query
    // silently returns an empty iterator — tests would pass while asserting
    // nothing — so this must not depend on a developer remembering to set
    // SLINT_EMIT_DEBUG_INFO=1.
    //
    // Debug builds only: the metadata inflates the generated code, and release
    // ships under the <100MB budget (CLAUDE.md § Scale & Performance).
    // `cargo test` builds debug, so tests always get it.
    let debug_info = std::env::var("PROFILE").as_deref() == Ok("debug");

    slint_build::compile_with_config(
        "ui/main.slint",
        slint_build::CompilerConfiguration::new()
            .with_style(style.into())
            .with_debug_info(debug_info),
    )
    .unwrap();
}
