#[path = "build_notices.rs"]
mod build_notices;

fn main() {
    // Third-party notices asset (About page). Naming the inputs also stops
    // Cargo from rerunning this script, and compiling the UI again, after
    // every edit in the package. Slint and winresource name their own inputs
    // for the UI files; the icon is listed here.
    let manifest_dir =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    for p in build_notices::rerun_paths(&manifest_dir) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rerun-if-changed=assets/icons/mello.ico");
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("out dir"));
    build_notices::generate(&manifest_dir, &out_dir.join("third_party_notices.txt"));

    // Set @executable_path as rpath so the binary finds libonnxruntime.dylib
    // next to itself (in .app bundle or in target/release/)
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");

    // Linux: $ORIGIN, so the binary finds libonnxruntime.so next to itself
    // (in the AppImage and in target/release/).
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");

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

    // The StatValue probe window for src/stat_value_tests.rs. Compiled first,
    // because the last compile_with_config call wins the
    // SLINT_INCLUDE_GENERATED variable that include_modules!() reads, and
    // that must stay main.slint. The test includes the probe by path.
    if debug_info {
        slint_build::compile_with_config(
            "ui/previews/stat_probe.slint",
            slint_build::CompilerConfiguration::new()
                .with_style(style.into())
                .with_debug_info(true),
        )
        .unwrap();
    }

    slint_build::compile_with_config(
        "ui/main.slint",
        slint_build::CompilerConfiguration::new()
            .with_style(style.into())
            .with_debug_info(debug_info),
    )
    .unwrap();
}
