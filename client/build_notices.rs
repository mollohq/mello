//! Build-time collector for the Third-party notices view.
//!
//! Writes `third_party_notices.txt` into `OUT_DIR`. The client embeds it with
//! `include_str!` and `src/about.rs` parses it.
//!
//! Format: records are separated by U+001E. Each record has three fields
//! separated by U+001F: the names (one `name version` pair per line), the
//! licence name, and the licence text. Dependencies with the same text share
//! one record, so the asset stays small.
//!
//! Sources:
//! - Rust crates: `cargo tree` on the shipped targets, normal dependencies
//!   only, and the LICENSE / COPYING files in the crate source.
//! - libmello: `libmello/vcpkg.json` and `libmello/third_party/*`.
//! - The OFL texts of Oxanium and Barlow, in `ui/fonts`.
//! - OpenMoji and Slint, by name.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const RECORD: char = '\u{1e}';
const FIELD: char = '\u{1f}';

/// Targets the client ships for. `cargo tree --target all` would add
/// Android and Wasm crates that never reach an install.
const TARGETS: [&str; 3] = [
    "x86_64-pc-windows-msvc",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
];

/// File names, lowercase prefix, that carry a licence text.
const LICENSE_PREFIXES: [&str; 4] = ["license", "licence", "copying", "unlicense"];

/// One dependency before grouping.
struct Dep {
    name: String,
    version: String,
    license: String,
    text: String,
}

/// Paths the build script must watch. Without these Cargo reruns the script
/// on every edit in the package, which compiles the Slint UI again.
pub fn rerun_paths(manifest_dir: &Path) -> Vec<PathBuf> {
    let root = manifest_dir.join("..");
    vec![
        root.join("Cargo.lock"),
        root.join("libmello/vcpkg.json"),
        root.join("libmello/third_party"),
        manifest_dir.join("ui/fonts"),
        manifest_dir.join("build_notices.rs"),
    ]
}

/// Collect the notices and write the asset. Returns the number of records.
pub fn generate(manifest_dir: &Path, out_file: &Path) -> usize {
    let root = manifest_dir.join("..");
    let mut deps = rust_crates(manifest_dir);
    deps.extend(libmello_deps(&root));
    deps.extend(bundled_assets(manifest_dir));

    // Group by identical text. BTreeMap keeps the output stable.
    let mut groups: BTreeMap<String, Vec<Dep>> = BTreeMap::new();
    for d in deps {
        groups.entry(d.text.clone()).or_default().push(d);
    }
    let mut records: Vec<(String, String)> = groups
        .into_iter()
        .map(|(text, mut members)| {
            members.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
            let mut names = String::new();
            let mut licenses: Vec<String> = Vec::new();
            for m in &members {
                if !names.is_empty() {
                    names.push('\n');
                }
                let _ = write!(names, "{} {}", m.name, m.version);
                if !licenses.contains(&m.license) {
                    licenses.push(m.license.clone());
                }
            }
            (
                members[0].name.to_lowercase(),
                format!(
                    "{names}{FIELD}{}{FIELD}{}",
                    licenses.join("; "),
                    text.trim()
                ),
            )
        })
        .collect();
    // Order by the first name in the group so the view reads alphabetically.
    records.sort();
    let count = records.len();
    let body: Vec<String> = records.into_iter().map(|(_, r)| r).collect();
    std::fs::write(out_file, body.join(&RECORD.to_string())).expect("write notices asset");
    count
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn registry_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| PathBuf::from(h).join(".cargo"))
        });
    let Some(src) = home.map(|h| h.join("registry/src")) else {
        return Vec::new();
    };
    std::fs::read_dir(src)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// The text of every licence file in a directory, joined.
fn license_text(dir: &Path) -> String {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    let mut out: Vec<String> = Vec::new();
    for f in files {
        let Some(name) = f.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_lowercase();
        if !f.is_file() || !LICENSE_PREFIXES.iter().any(|p| lower.starts_with(p)) {
            continue;
        }
        if let Ok(t) = std::fs::read_to_string(&f) {
            let t = t.replace("\r\n", "\n");
            out.push(if out.is_empty() {
                t.trim().to_string()
            } else {
                format!("{name}\n\n{}", t.trim())
            });
        }
    }
    out.join("\n\n")
}

/// True when every licence in the expression is MIT or Apache-2.0 (with or
/// without the LLVM exception). Their texts differ only by the copyright
/// line, so the asset keeps that line and one copy of each standard text.
fn is_mit_apache(expr: &str) -> bool {
    expr.split(|c: char| c == '/' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .all(|t| {
            matches!(
                t,
                "MIT" | "Apache-2.0" | "OR" | "AND" | "WITH" | "LLVM-exception" | "(" | ")"
            )
        })
}

/// The copyright lines of a licence text, without duplicates.
fn copyright_lines(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for l in text.lines() {
        let t = l.trim();
        let lower = t.to_lowercase();
        // A holder line has a year, "(c)" or the (c) sign. The Apache text
        // has "copyright" lines of its own, and its appendix a "[yyyy]" stub.
        let holder = lower.contains("(c)")
            || lower.contains('\u{a9}')
            || lower.chars().any(|c| c.is_ascii_digit());
        if lower.starts_with("copyright")
            && holder
            && !t.contains('[')
            && !lines.iter().any(|x| x == t)
        {
            lines.push(t.to_string());
        }
    }
    lines.join("\n")
}

/// First file in `dir` whose name contains `needle`, for the standard texts.
fn file_text_named(dir: &Path, needle: &str) -> Option<String> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().to_lowercase();
        (n.contains("licen") && n.contains(needle) && e.path().is_file())
            .then(|| std::fs::read_to_string(e.path()).ok())
            .flatten()
            .map(|t| t.replace("\r\n", "\n").trim().to_string())
    })
}

fn rust_crates(manifest_dir: &Path) -> Vec<Dep> {
    let mut cmd = Command::new(cargo());
    cmd.current_dir(manifest_dir).args([
        "tree",
        "-p",
        "mello-client",
        "-e",
        "normal",
        "--offline",
        "--prefix",
        "none",
        "--format",
        "{p}|{l}",
    ]);
    for t in TARGETS {
        cmd.args(["--target", t]);
    }
    let out = cmd.output().expect("run cargo tree");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let roots = registry_roots();
    let mut seen = std::collections::BTreeSet::new();
    let mut deps = Vec::new();
    let mut std_apache: Option<String> = None;
    let mut std_mit: Option<String> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_end_matches(" (*)");
        let Some((pkg, license)) = line.split_once('|') else {
            continue;
        };
        if pkg.contains("(proc-macro)") || !seen.insert(line.to_string()) {
            continue;
        }
        let mut parts = pkg.split_whitespace();
        let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        let version = version.trim_start_matches('v');
        let rest = parts.collect::<Vec<_>>().join(" ");
        // A path in parentheses is a local crate: ours or a patched one.
        let local = rest
            .strip_prefix('(')
            .and_then(|r| r.strip_suffix(')'))
            .map(PathBuf::from);
        let dir = match &local {
            Some(p) => Some(p.clone()),
            None => roots
                .iter()
                .map(|r| r.join(format!("{name}-{version}")))
                .find(|p| p.is_dir()),
        };
        // Our own crates are Mollo Tech code, covered by the m3llo licence.
        if local
            .as_ref()
            .is_some_and(|p| !p.to_string_lossy().contains("third_party"))
        {
            continue;
        }
        let license = if license.is_empty() {
            "unspecified"
        } else {
            license
        };
        let mut text = dir.as_deref().map(license_text).unwrap_or_default();
        if is_mit_apache(license) && !text.is_empty() {
            if let Some(d) = dir.as_deref() {
                if std_apache.is_none() && license.contains("Apache-2.0") {
                    std_apache = file_text_named(d, "apache");
                }
                if std_mit.is_none() && license.contains("MIT") {
                    std_mit = file_text_named(d, "mit");
                }
            }
            let c = copyright_lines(&text);
            text = if c.is_empty() {
                format!("Licence: {license}. The licence file names no copyright holder.")
            } else {
                c
            };
        } else if text.is_empty() {
            text = format!(
                "The package ships no licence file. Licence: {license}.\nThe SPDX text is at https://spdx.org/licenses/."
            );
        }
        deps.push(Dep {
            name: name.to_string(),
            version: version.to_string(),
            license: license.to_string(),
            text,
        });
    }
    for (name, text) in [("Apache-2.0", std_apache), ("MIT", std_mit)] {
        if let Some(text) = text {
            deps.push(Dep {
                name: format!("{name} licence text"),
                version: "".into(),
                license: name.into(),
                text: format!(
                    "The standard {name} text. It applies to every crate above that lists {name}. The copyright holders are listed with each crate.\n\n{text}"
                ),
            });
        }
    }
    deps
}

/// Licence names of the vcpkg ports in `libmello/vcpkg.json` that ship.
/// vcpkg is not installed on every build machine, so the names live here.
const VCPKG_LICENSES: [(&str, &str); 4] = [
    ("opus", "BSD-3-Clause"),
    ("openssl", "Apache-2.0"),
    ("libdatachannel", "MPL-2.0"),
    ("abseil", "Apache-2.0"),
];

fn libmello_deps(root: &Path) -> Vec<Dep> {
    let mut deps = Vec::new();
    let manifest = std::fs::read_to_string(root.join("libmello/vcpkg.json")).unwrap_or_default();
    for (port, license) in VCPKG_LICENSES {
        if manifest.contains(&format!("\"{port}\"")) {
            deps.push(Dep {
                name: port.into(),
                version: "(vcpkg)".into(),
                license: license.into(),
                text: format!(
                    "Licence: {license}. The full text is at https://spdx.org/licenses/{license}.html and in the {port} source."
                ),
            });
        }
    }
    // Libraries vendored under third_party, with the licence file they carry.
    for (dir, name, license) in [
        ("rnnoise", "RNNoise", "BSD-3-Clause"),
        (
            "webrtc-audio-processing",
            "WebRTC audio processing",
            "BSD-3-Clause",
        ),
        (
            "webrtc-goog-cc",
            "WebRTC congestion control",
            "BSD-3-Clause",
        ),
        ("webrtc-neteq", "WebRTC NetEQ", "BSD-3-Clause"),
        ("webrtc-video-timing", "WebRTC video timing", "BSD-3-Clause"),
    ] {
        let text = license_text(&root.join("libmello/third_party").join(dir));
        if !text.is_empty() {
            deps.push(Dep {
                name: name.into(),
                version: "(libmello)".into(),
                license: license.into(),
                text,
            });
        }
    }
    // ONNX Runtime ships its licence inside a versioned folder.
    if let Ok(rd) = std::fs::read_dir(root.join("libmello/third_party/onnxruntime")) {
        for e in rd.flatten() {
            let text = license_text(&e.path());
            if text.is_empty() {
                continue;
            }
            let version = e
                .file_name()
                .to_string_lossy()
                .rsplit('-')
                .next()
                .unwrap_or("")
                .to_string();
            deps.push(Dep {
                name: "ONNX Runtime".into(),
                version,
                license: "MIT".into(),
                text,
            });
            break;
        }
    }
    deps.push(Dep {
        name: "OpenH264".into(),
        version: "(Cisco binary)".into(),
        license: "BSD-2-Clause".into(),
        text: "OpenH264 is used as the prebuilt binary from Cisco. Licence: BSD-2-Clause.\nSource and licence: https://github.com/cisco/openh264".into(),
    });
    deps.push(Dep {
        name: "Silero VAD".into(),
        version: "".into(),
        license: "MIT".into(),
        text: "Licence: MIT. Source: https://github.com/snakers4/silero-vad".into(),
    });
    deps
}

fn bundled_assets(manifest_dir: &Path) -> Vec<Dep> {
    let fonts = manifest_dir.join("ui/fonts");
    let mut deps = Vec::new();
    for (name, file) in [("Oxanium", "OFL-Oxanium.txt"), ("Barlow", "OFL-Barlow.txt")] {
        let text = std::fs::read_to_string(fonts.join(file))
            .unwrap_or_else(|_| panic!("missing {file} in ui/fonts"))
            .replace("\r\n", "\n");
        deps.push(Dep {
            name: name.into(),
            version: "".into(),
            license: "OFL-1.1".into(),
            text,
        });
    }
    deps.push(Dep {
        name: "OpenMoji".into(),
        version: "".into(),
        license: "CC-BY-SA-4.0".into(),
        text: "Emoji by OpenMoji (https://openmoji.org), CC BY-SA 4.0.\nLicence: https://creativecommons.org/licenses/by-sa/4.0/".into(),
    });
    deps.push(Dep {
        name: "Slint".into(),
        version: "".into(),
        license: "Slint Royalty-free 2.0".into(),
        text: "Made with Slint (https://slint.dev), used under the Slint Royalty-free licence."
            .into(),
    });
    deps
}
