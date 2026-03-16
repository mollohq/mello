use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../libmello/include/mello.h");
    println!("cargo:rerun-if-changed=../libmello/src/");
    println!("cargo:rerun-if-changed=../libmello/CMakeLists.txt");
    println!("cargo:rerun-if-changed=../libmello/vcpkg.json");
    println!("cargo:rerun-if-changed=../libmello/third_party/");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    // Locate vcpkg inside the repo (external/vcpkg submodule)
    let vcpkg_root = Path::new(&manifest_dir).join("../external/vcpkg");
    let vcpkg_root = strip_win_prefix(
        &vcpkg_root
            .canonicalize()
            .expect("external/vcpkg not found — run: git submodule update --init"),
    );

    // Bootstrap vcpkg if needed
    bootstrap_vcpkg(&vcpkg_root);

    let toolchain = vcpkg_root.join("scripts/buildsystems/vcpkg.cmake");

    let triplet = match (target_os.as_str(), target_arch.as_str()) {
        ("windows", _) => "x64-windows-static-md",
        ("macos", "aarch64") => "arm64-osx",
        ("macos", _) => "x64-osx",
        _ => "x64-linux",
    };

    let mut cmake_cfg = cmake::Config::new("../libmello");
    cmake_cfg
        .define("CMAKE_TOOLCHAIN_FILE", toolchain.to_str().unwrap())
        .define("VCPKG_TARGET_TRIPLET", triplet)
        .profile("Release");

    // On macOS arm64, explicitly set architecture to avoid cross-compilation issues
    if target_os == "macos" && target_arch == "aarch64" {
        cmake_cfg.define("CMAKE_OSX_ARCHITECTURES", "arm64");
        cmake_cfg.define("VCPKG_HOST_TRIPLET", "arm64-osx");
    }

    let dst = cmake_cfg.build();

    // Link libmello + rnnoise (built by cmake)
    let lib_dir = dst.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=mello");
    println!("cargo:rustc-link-lib=static=rnnoise");

    // In manifest mode, vcpkg installs into the cmake build dir
    let out_dir = env::var("OUT_DIR").unwrap();
    let vcpkg_installed = Path::new(&out_dir).join("build/vcpkg_installed").join(triplet).join("lib");
    println!("cargo:rustc-link-search=native={}", vcpkg_installed.display());
    println!("cargo:rustc-link-lib=static=opus");
    println!("cargo:rustc-link-lib=static=datachannel");
    println!("cargo:rustc-link-lib=static=juice");
    println!("cargo:rustc-link-lib=static=usrsctp");

    // dav1d AV1 decoder (statically linked via vcpkg)
    if vcpkg_installed.join("dav1d.lib").exists() || vcpkg_installed.join("libdav1d.a").exists() {
        println!("cargo:rustc-link-lib=static=dav1d");
    }

    // ONNX Runtime (dynamic linking — static is 2GB+ and impractical)
    let ort_subdir = match (target_os.as_str(), target_arch.as_str()) {
        ("windows", _) => "onnxruntime-win-x64-1.23.2",
        ("macos", "aarch64") => "onnxruntime-osx-arm64-1.23.0",
        ("macos", _) => "onnxruntime-osx-x86_64-1.23.0",
        _ => "onnxruntime-linux-x64-1.23.0",
    };
    let ort_dir = Path::new(&manifest_dir)
        .join(format!("../libmello/third_party/onnxruntime/{}", ort_subdir));
    let ort_dir = strip_win_prefix(
        &ort_dir
            .canonicalize()
            .expect("onnxruntime prebuilt dir not found — run scripts/setup-macos.sh (or equivalent)"),
    );
    let ort_lib = ort_dir.join("lib");
    println!("cargo:rustc-link-search=native={}", ort_lib.display());
    println!("cargo:rustc-link-lib=dylib=onnxruntime");

    // Copy shared libraries next to the output binary so cargo run works
    let target_dir = Path::new(&out_dir)
        .ancestors()
        .find(|p| p.ends_with("debug") || p.ends_with("release"))
        .map(|p| p.join("deps"))
        .unwrap_or_else(|| PathBuf::from(&out_dir));

    match target_os.as_str() {
        "windows" => {
            for dll in &["onnxruntime.dll", "onnxruntime_providers_shared.dll"] {
                let src = ort_lib.join(dll);
                if src.exists() {
                    let _ = std::fs::copy(&src, target_dir.join(dll));
                    if let Some(parent) = target_dir.parent() {
                        let _ = std::fs::copy(&src, parent.join(dll));
                    }
                }
            }

            // OpenH264 Cisco prebuilt DLL (runtime-loaded, not linked)
            let oh264_dir = Path::new(&manifest_dir).join("../libmello/third_party/openh264");
            if oh264_dir.exists() {
                for dll in &["openh264-2.6.0-win64.dll", "openh264-2.5.0-win64.dll", "openh264.dll"] {
                    let src = oh264_dir.join(dll);
                    if src.exists() {
                        let _ = std::fs::copy(&src, target_dir.join(dll));
                        if let Some(parent) = target_dir.parent() {
                            let _ = std::fs::copy(&src, parent.join(dll));
                        }
                    }
                }
            }
        }
        "macos" => {
            // Copy both the versioned dylib and the unversioned symlink
            for dylib in &["libonnxruntime.dylib", "libonnxruntime.1.23.0.dylib"] {
                let src = ort_lib.join(dylib);
                if src.exists() {
                    let _ = std::fs::copy(&src, target_dir.join(dylib));
                    if let Some(parent) = target_dir.parent() {
                        let _ = std::fs::copy(&src, parent.join(dylib));
                    }
                }
            }
        }
        _ => {
            {
                let so = "libonnxruntime.so";
                let src = ort_lib.join(so);
                if src.exists() {
                    let _ = std::fs::copy(&src, target_dir.join(so));
                    if let Some(parent) = target_dir.parent() {
                        let _ = std::fs::copy(&src, parent.join(so));
                    }
                }
            }
        }
    }

    // Platform-specific system libraries
    match target_os.as_str() {
        "windows" => {
            // OpenSSL from vcpkg (Windows names the libs with "lib" prefix)
            println!("cargo:rustc-link-lib=static=libssl");
            println!("cargo:rustc-link-lib=static=libcrypto");
            for lib in &[
                "ole32", "winmm", "ksuser", "mfplat", "mfuuid", "avrt",
                "ws2_32", "crypt32", "bcrypt", "user32", "advapi32",
                "d3d11", "dxgi", "dxguid", "d3dcompiler",
                "windowsapp",
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
            // C++ standard library
            println!("cargo:rustc-link-lib=dylib=c++");
        }
        _ => {
            println!("cargo:rustc-link-lib=static=ssl");
            println!("cargo:rustc-link-lib=static=crypto");
            println!("cargo:rustc-link-lib=dylib=asound");
            println!("cargo:rustc-link-lib=dylib=pulse");
            println!("cargo:rustc-link-lib=dylib=pthread");
            println!("cargo:rustc-link-lib=dylib=stdc++");
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

/// Bootstrap vcpkg if the binary doesn't exist yet.
fn bootstrap_vcpkg(vcpkg_root: &Path) {
    let vcpkg_bin = if cfg!(target_os = "windows") {
        vcpkg_root.join("vcpkg.exe")
    } else {
        vcpkg_root.join("vcpkg")
    };

    if vcpkg_bin.exists() {
        return;
    }

    eprintln!("Bootstrapping vcpkg at {} ...", vcpkg_root.display());

    let script = if cfg!(target_os = "windows") {
        vcpkg_root.join("bootstrap-vcpkg.bat")
    } else {
        vcpkg_root.join("bootstrap-vcpkg.sh")
    };

    let status = Command::new(&script)
        .arg("-disableMetrics")
        .current_dir(vcpkg_root)
        .status()
        .expect("failed to run vcpkg bootstrap script");

    assert!(status.success(), "vcpkg bootstrap failed");
}

#[cfg(target_os = "windows")]
fn strip_win_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\?\") {
        PathBuf::from(&s[4..])
    } else {
        p.to_path_buf()
    }
}

#[cfg(not(target_os = "windows"))]
fn strip_win_prefix(p: &Path) -> PathBuf {
    p.to_path_buf()
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
