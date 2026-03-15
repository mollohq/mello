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
    } else if cfg!(target_os = "windows") {
        "fluent"
    } else {
        "fluent" // Linux fallback
    };

    slint_build::compile_with_config(
        "ui/main.slint",
        slint_build::CompilerConfiguration::new().with_style(style.into()),
    )
    .unwrap();
}
