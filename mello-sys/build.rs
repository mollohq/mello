use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../libmello/include/mello.h");
    println!("cargo:rerun-if-changed=../libmello/src/");
    println!("cargo:rerun-if-changed=../libmello/CMakeLists.txt");

    let vcpkg_root = env::var("VCPKG_ROOT").unwrap_or_else(|_| "C:\\vcpkg".into());
    let toolchain = format!("{}/scripts/buildsystems/vcpkg.cmake", vcpkg_root);
    let triplet = "x64-windows-static-md";

    let dst = cmake::Config::new("../libmello")
        .define("CMAKE_TOOLCHAIN_FILE", &toolchain)
        .define("VCPKG_TARGET_TRIPLET", triplet)
        .profile("Release")
        .build();

    let lib_dir = dst.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=mello");
    println!("cargo:rustc-link-lib=static=rnnoise");

    let vcpkg_lib = format!("{}/installed/{}/lib", vcpkg_root, triplet);
    println!("cargo:rustc-link-search=native={}", vcpkg_lib);
    println!("cargo:rustc-link-lib=static=opus");
    println!("cargo:rustc-link-lib=static=datachannel");
    println!("cargo:rustc-link-lib=static=juice");
    println!("cargo:rustc-link-lib=static=usrsctp");
    println!("cargo:rustc-link-lib=static=libssl");
    println!("cargo:rustc-link-lib=static=libcrypto");

    // Windows system libs required by WASAPI, COM, and OpenSSL
    println!("cargo:rustc-link-lib=dylib=ole32");
    println!("cargo:rustc-link-lib=dylib=winmm");
    println!("cargo:rustc-link-lib=dylib=ksuser");
    println!("cargo:rustc-link-lib=dylib=mfplat");
    println!("cargo:rustc-link-lib=dylib=mfuuid");
    println!("cargo:rustc-link-lib=dylib=avrt");
    println!("cargo:rustc-link-lib=dylib=ws2_32");
    println!("cargo:rustc-link-lib=dylib=crypt32");
    println!("cargo:rustc-link-lib=dylib=bcrypt");
    println!("cargo:rustc-link-lib=dylib=user32");
    println!("cargo:rustc-link-lib=dylib=advapi32");

    // Generate Rust bindings from mello.h
    let bindings = bindgen::Builder::default()
        .header("../libmello/include/mello.h")
        .allowlist_function("mello_.*")
        .allowlist_type("Mello.*")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Failed to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Failed to write bindings");
}
