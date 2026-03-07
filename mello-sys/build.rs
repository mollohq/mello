use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../libmello/include/mello.h");
    println!("cargo:rerun-if-changed=../libmello/src/");
    println!("cargo:rerun-if-changed=../libmello/CMakeLists.txt");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();

    let vcpkg_root = env::var("VCPKG_ROOT")
        .expect("VCPKG_ROOT must be set (e.g. c:\\dev\\vcpkg or ~/vcpkg)");
    let toolchain = format!("{}/scripts/buildsystems/vcpkg.cmake", vcpkg_root);

    let triplet = match target_os.as_str() {
        "windows" => "x64-windows-static-md",
        "macos" => "x64-osx",
        _ => "x64-linux",
    };

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

    match target_os.as_str() {
        "windows" => {
            println!("cargo:rustc-link-lib=static=libssl");
            println!("cargo:rustc-link-lib=static=libcrypto");
            for lib in &[
                "ole32", "winmm", "ksuser", "mfplat", "mfuuid", "avrt",
                "ws2_32", "crypt32", "bcrypt", "user32", "advapi32",
            ] {
                println!("cargo:rustc-link-lib=dylib={}", lib);
            }
        }
        "macos" => {
            println!("cargo:rustc-link-lib=static=ssl");
            println!("cargo:rustc-link-lib=static=crypto");
            println!("cargo:rustc-link-lib=framework=AudioToolbox");
            println!("cargo:rustc-link-lib=framework=CoreAudio");
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
            println!("cargo:rustc-link-lib=framework=Security");
        }
        _ => {
            println!("cargo:rustc-link-lib=static=ssl");
            println!("cargo:rustc-link-lib=static=crypto");
            println!("cargo:rustc-link-lib=dylib=asound");
            println!("cargo:rustc-link-lib=dylib=pulse");
            println!("cargo:rustc-link-lib=dylib=pthread");
        }
    }

    // Auto-detect libclang for bindgen if LIBCLANG_PATH not set
    if env::var("LIBCLANG_PATH").is_err() {
        if let Some(path) = find_libclang() {
            env::set_var("LIBCLANG_PATH", path);
        }
    }

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

fn find_libclang() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let candidates = [
            r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin",
            r"C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin",
            r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\Llvm\x64\bin",
            r"C:\Program Files\LLVM\bin",
        ];
        for dir in &candidates {
            if Path::new(dir).join("libclang.dll").exists() {
                return Some(dir.to_string());
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let candidates = [
            "/Library/Developer/CommandLineTools/usr/lib",
            "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib",
            "/opt/homebrew/opt/llvm/lib",
            "/usr/local/opt/llvm/lib",
        ];
        for dir in &candidates {
            if Path::new(dir).join("libclang.dylib").exists() {
                return Some(dir.to_string());
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        for ver in &["18", "17", "16", "15", "14"] {
            let dir = format!("/usr/lib/llvm-{}/lib", ver);
            if Path::new(&dir).join("libclang.so").exists() {
                return Some(dir);
            }
        }
        if Path::new("/usr/lib64").join("libclang.so").exists() {
            return Some("/usr/lib64".into());
        }
        if Path::new("/usr/lib").join("libclang.so").exists() {
            return Some("/usr/lib".into());
        }
    }

    None
}
