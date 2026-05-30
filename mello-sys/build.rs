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

    // iOS: cross-compile libmello via the vcpkg toolchain. Step 1 links the voice
    // DSP + transport for real; audio I/O, Silero VAD, and video are stubbed (no
    // RemoteIO/ORT/VideoToolbox yet). See mello-ios/specs/IOS-LIBMELLO-PORT.md §1a.
    if target_os == "ios" {
        build_ios(&manifest_dir);
        run_bindgen(&manifest_dir);
        return;
    }

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

    if target_os == "macos" {
        if target_arch == "aarch64" {
            cmake_cfg.define("CMAKE_OSX_ARCHITECTURES", "arm64");
            cmake_cfg.define("VCPKG_HOST_TRIPLET", "arm64-osx");
        }
        // The Rust `cmake` crate reads MACOSX_DEPLOYMENT_TARGET to set
        // -mmacosx-version-min in CMAKE_C_FLAGS/CMAKE_CXX_FLAGS.
        // Without this, it defaults to the SDK version (e.g. 26.2),
        // causing ObjC code to emit runtime features unavailable on
        // the actual OS (e.g. macOS 15).
        env::set_var("MACOSX_DEPLOYMENT_TARGET", "15.0");
        cmake_cfg.define("CMAKE_OSX_DEPLOYMENT_TARGET", "15.0");
    }

    let dst = cmake_cfg.build();

    // Link libmello + rnnoise + webrtc_audio_processing (built by cmake)
    let lib_dir = dst.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=mello");
    println!("cargo:rustc-link-lib=static=rnnoise");
    println!("cargo:rustc-link-lib=static=webrtc_audio_processing");

    // In manifest mode, vcpkg installs into the cmake build dir
    let out_dir = env::var("OUT_DIR").unwrap();
    let vcpkg_installed = Path::new(&out_dir)
        .join("build/vcpkg_installed")
        .join(triplet)
        .join("lib");
    println!(
        "cargo:rustc-link-search=native={}",
        vcpkg_installed.display()
    );
    println!("cargo:rustc-link-lib=static=opus");
    println!("cargo:rustc-link-lib=static=datachannel");
    println!("cargo:rustc-link-lib=static=juice");
    println!("cargo:rustc-link-lib=static=srtp2");
    println!("cargo:rustc-link-lib=static=usrsctp");

    // Abseil (transitive dependency of webrtc_audio_processing, installed by vcpkg)
    if let Ok(entries) = std::fs::read_dir(&vcpkg_installed) {
        let suffix = if target_os == "windows" { ".lib" } else { ".a" };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("absl_") && name_str.ends_with(suffix) {
                let lib_name = name_str.strip_suffix(suffix).unwrap();
                println!("cargo:rustc-link-lib=static={}", lib_name);
            }
        }
    }

    // dav1d AV1 decoder (statically linked via vcpkg)
    if vcpkg_installed.join("dav1d.lib").exists() || vcpkg_installed.join("libdav1d.a").exists() {
        println!("cargo:rustc-link-lib=static=dav1d");
    }

    // ONNX Runtime (dynamic linking — static is 2GB+ and impractical)
    // CMake downloads ORT into third_party/onnxruntime/<platform-dir>/; find it by glob
    // so we don't duplicate the version string here.
    let ort_prefix = match (target_os.as_str(), target_arch.as_str()) {
        ("windows", _) => "onnxruntime-win-x64-",
        ("macos", "aarch64") => "onnxruntime-osx-arm64-",
        ("macos", _) => "onnxruntime-osx-x86_64-",
        _ => "onnxruntime-linux-x64-",
    };
    let ort_base = Path::new(&manifest_dir).join("../libmello/third_party/onnxruntime");
    let ort_dir = std::fs::read_dir(&ort_base)
        .expect("third_party/onnxruntime dir not found — CMake should have created it")
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().starts_with(ort_prefix))
        .map(|e| e.path())
        .expect("onnxruntime prebuilt dir not found after CMake build");
    let ort_dir = strip_win_prefix(
        &ort_dir
            .canonicalize()
            .expect("failed to canonicalize onnxruntime path"),
    );
    let ort_lib = ort_dir.join("lib");
    println!("cargo:rustc-link-search=native={}", ort_lib.display());
    println!("cargo:rustc-link-lib=dylib=onnxruntime");

    // Windows: delay-load onnxruntime.dll so it isn't pulled in at process startup.
    // Windows ships its own older copy in WinSxS (Copilot, Studio Effects) which
    // shadows ours. With delay-load, the DLL isn't loaded until the first ORT API
    // call, giving vad.cpp a chance to LoadLibraryW our copy by absolute path first.
    if target_os == "windows" {
        println!("cargo:rustc-link-arg=delayimp.lib");
        println!("cargo:rustc-link-arg=/DELAYLOAD:onnxruntime.dll");
    }

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
                for dll in &[
                    "openh264-2.6.0-win64.dll",
                    "openh264-2.5.0-win64.dll",
                    "openh264.dll",
                ] {
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
            // Copy all libonnxruntime dylibs (versioned + unversioned symlink)
            if let Ok(entries) = std::fs::read_dir(&ort_lib) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with("libonnxruntime") && name_str.ends_with(".dylib") {
                        let _ = copy_file_or_symlink(&entry.path(), &target_dir.join(&name));
                        if let Some(parent) = target_dir.parent() {
                            let _ = copy_file_or_symlink(&entry.path(), &parent.join(&name));
                        }
                    }
                }
            }
        }
        _ => {
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

    // Platform-specific system libraries
    match target_os.as_str() {
        "windows" => {
            // OpenSSL from vcpkg (Windows names the libs with "lib" prefix)
            println!("cargo:rustc-link-lib=static=libssl");
            println!("cargo:rustc-link-lib=static=libcrypto");
            for lib in &[
                "ole32",
                "winmm",
                "ksuser",
                "mfplat",
                "mfreadwrite",
                "mfuuid",
                "avrt",
                "ws2_32",
                "crypt32",
                "bcrypt",
                "user32",
                "advapi32",
                "d3d11",
                "dxgi",
                "dxguid",
                "d3dcompiler",
                "windowsapp",
                "gdi32",
            ] {
                println!("cargo:rustc-link-lib=dylib={}", lib);
            }
        }
        "macos" => {
            println!("cargo:rustc-link-lib=static=ssl");
            println!("cargo:rustc-link-lib=static=crypto");
            // Audio
            println!("cargo:rustc-link-lib=framework=AudioToolbox");
            println!("cargo:rustc-link-lib=framework=CoreAudio");
            println!("cargo:rustc-link-lib=framework=AVFoundation");
            // Video / Streaming
            println!("cargo:rustc-link-lib=framework=Metal");
            println!("cargo:rustc-link-lib=framework=VideoToolbox");
            println!("cargo:rustc-link-lib=framework=CoreMedia");
            println!("cargo:rustc-link-lib=framework=CoreVideo");
            println!("cargo:rustc-link-lib=framework=CoreGraphics");
            println!("cargo:rustc-link-lib=framework=ScreenCaptureKit");
            println!("cargo:rustc-link-lib=framework=AppKit");
            println!("cargo:rustc-link-lib=framework=Accelerate");
            // System
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

    run_bindgen(&manifest_dir);
}

/// Generate the Rust bindings from `mello.h`. Shared by the native build and the
/// iOS stub path so both produce identical types for `mello-core`.
fn run_bindgen(manifest_dir: &str) {
    // Auto-detect libclang for bindgen if LIBCLANG_PATH not set
    if env::var("LIBCLANG_PATH").is_err() {
        if let Some(path) = find_libclang() {
            env::set_var("LIBCLANG_PATH", path);
        }
    }

    let header = Path::new(manifest_dir).join("../libmello/include/mello.h");
    let bindings = bindgen::Builder::default()
        .header(header.to_str().unwrap())
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

/// Cross-compile libmello for iOS (device `aarch64-apple-ios` or simulator
/// `aarch64-apple-ios-sim`) through the vcpkg toolchain, then emit link directives.
///
/// Steps 1–2 (IOS-LIBMELLO-PORT.md §1a): the voice DSP (opus/rnnoise/webrtc-apm),
/// transport (libdatachannel), and Silero VAD (prebuilt static ONNX Runtime
/// xcframework) link for real; the audio I/O backend and video decode are still
/// stubbed (gated by MELLO_IOS_NO_VIDEO in the CMake iOS branch). usrsctp needs an
/// iOS patch, supplied via the overlay port.
fn build_ios(manifest_dir: &str) {
    // Both iOS targets report target_os = "ios"; the simulator triple ends in -sim.
    let target = env::var("TARGET").unwrap();
    let is_sim = target.ends_with("-sim");
    let (triplet, sysroot) = if is_sim {
        ("arm64-ios-simulator", "iphonesimulator")
    } else {
        ("arm64-ios", "iphoneos")
    };

    let vcpkg_root = Path::new(manifest_dir).join("../external/vcpkg");
    let vcpkg_root = strip_win_prefix(
        &vcpkg_root
            .canonicalize()
            .expect("external/vcpkg not found — run: git submodule update --init"),
    );
    bootstrap_vcpkg(&vcpkg_root);
    let toolchain = vcpkg_root.join("scripts/buildsystems/vcpkg.cmake");

    // usrsctp's userspace route monitor uses <net/route.h> (absent on iOS) and
    // misses the Apple RFC define on CMAKE_SYSTEM_NAME=iOS; our overlay port
    // patches both so libdatachannel's SCTP dep cross-compiles.
    let overlay = Path::new(manifest_dir).join("../libmello/vcpkg-overlays");
    let overlay = strip_win_prefix(
        &overlay
            .canonicalize()
            .expect("libmello/vcpkg-overlays not found"),
    );

    let mut cmake_cfg = cmake::Config::new("../libmello");
    cmake_cfg
        .define("CMAKE_TOOLCHAIN_FILE", toolchain.to_str().unwrap())
        .define("VCPKG_TARGET_TRIPLET", triplet)
        .define("VCPKG_HOST_TRIPLET", "arm64-osx")
        .define("VCPKG_OVERLAY_PORTS", overlay.to_str().unwrap())
        .define("CMAKE_SYSTEM_NAME", "iOS")
        .define("CMAKE_OSX_ARCHITECTURES", "arm64")
        .define("CMAKE_OSX_SYSROOT", sysroot)
        .define("CMAKE_OSX_DEPLOYMENT_TARGET", "18.0")
        // Selects the ORT xcframework slice (device vs simulator) deterministically
        // instead of pattern-matching the resolved CMAKE_OSX_SYSROOT path.
        .define("MELLO_IOS_SIMULATOR", if is_sim { "ON" } else { "OFF" })
        .profile("Release");

    let dst = cmake_cfg.build();

    // libmello + sibling static libs built by cmake.
    let lib_dir = dst.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=mello");
    println!("cargo:rustc-link-lib=static=rnnoise");
    println!("cargo:rustc-link-lib=static=webrtc_audio_processing");

    // Prebuilt static ONNX Runtime xcframework (downloaded + SHA256-verified by the
    // CMake iOS branch). The framework binary is a static archive; link it as a
    // framework so the linker resolves Silero VAD's ORT symbols. The app's real link
    // goes through build-core.sh (which merges this archive in); this keeps any
    // cargo-driven iOS link self-consistent.
    let ort_slice = if is_sim {
        "ios-arm64_x86_64-simulator"
    } else {
        "ios-arm64"
    };
    let ort_fw_dir = Path::new(manifest_dir)
        .join("../libmello/third_party/onnxruntime-ios/onnxruntime.xcframework")
        .join(ort_slice);
    println!("cargo:rustc-link-search=framework={}", ort_fw_dir.display());
    println!("cargo:rustc-link-lib=framework=onnxruntime");

    // vcpkg deps (manifest mode installs into the cmake build dir).
    let out_dir = env::var("OUT_DIR").unwrap();
    let vcpkg_installed = Path::new(&out_dir)
        .join("build/vcpkg_installed")
        .join(triplet)
        .join("lib");
    println!(
        "cargo:rustc-link-search=native={}",
        vcpkg_installed.display()
    );
    for lib in &[
        "opus",
        "datachannel",
        "juice",
        "srtp2",
        "usrsctp",
        "ssl",
        "crypto",
    ] {
        println!("cargo:rustc-link-lib=static={}", lib);
    }

    // Abseil (transitive dependency of webrtc_audio_processing).
    if let Ok(entries) = std::fs::read_dir(&vcpkg_installed) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("absl_") && name_str.ends_with(".a") {
                let lib_name = name_str.strip_suffix(".a").unwrap();
                println!("cargo:rustc-link-lib=static={}", lib_name);
            }
        }
    }

    // System frameworks + libc++. The app's final link happens in Xcode (which
    // does not read these), so the xcframework packaging links them too; emitting
    // here keeps any cargo-driven iOS link (tests/examples) self-consistent.
    for framework in &[
        "AudioToolbox",
        "AVFoundation",
        "VideoToolbox",
        "CoreMedia",
        "CoreVideo",
        "CoreFoundation",
        "CoreGraphics",
        "Metal",
        "Accelerate",
        "Security",
    ] {
        println!("cargo:rustc-link-lib=framework={}", framework);
    }
    println!("cargo:rustc-link-lib=dylib=c++");
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
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
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

fn copy_file_or_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(src)?;
    if metadata.file_type().is_symlink() {
        if std::fs::symlink_metadata(dst).is_ok() {
            let _ = std::fs::remove_file(dst);
        }
        #[cfg(unix)]
        {
            let target = std::fs::read_link(src)?;
            std::os::unix::fs::symlink(target, dst)?;
        }
        #[cfg(not(unix))]
        {
            let resolved = src.canonicalize()?;
            let _ = std::fs::copy(resolved, dst)?;
        }
        return Ok(());
    }

    let _ = std::fs::copy(src, dst)?;
    Ok(())
}
