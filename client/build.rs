fn main() {
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
