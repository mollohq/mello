fn main() {
    // TODO: Generate bindings from mello.h using bindgen
    // TODO: Link to libmello
    
    println!("cargo:rerun-if-changed=../libmello/include/mello.h");
    
    // For now, just create empty bindings
    let out_path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(
        out_path.join("bindings.rs"),
        "// TODO: Generated bindings will go here\n"
    ).unwrap();
}
