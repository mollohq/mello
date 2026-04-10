fn main() {
    slint_build::compile_with_config(
        "ui/mini_player.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent".into()),
    )
    .unwrap();
}
