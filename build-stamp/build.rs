// Build stamp: which commit made this binary. Read at run time by
// `mello --build-info` (client/src/build_info.rs). A source tarball has no git,
// so every git call can fail; the build must still succeed.
//
// This script lives in its own crate, not in client/build.rs. It lists the
// client sources in `rerun-if-changed`. In the client's own script, that list
// would run the Slint compiler again after every Rust edit.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let commit = git(&["rev-parse", "HEAD"])
        .filter(|c| matches!(c.len(), 40 | 64) && c.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_string());

    // Release CI rewrites three tracked files before it builds: the workspace
    // version in Cargo.toml, the matching entry in Cargo.lock, and the version
    // in client/macos/Info.plist. Those edits are not source changes, so they
    // must not make the stamp dirty. The exclusion applies on GitHub Actions
    // only. On a developer machine an edit to any of these files is real.
    //
    // `--no-optional-locks`: `git status` must not rewrite the index, or the
    // index would change on every build and cargo would rerun this script.
    // With no pathspec, status covers the whole repo, whatever the current
    // directory is. With only exclude pathspecs, it covers everything else.
    let mut status = vec!["--no-optional-locks", "status", "--porcelain"];
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        status.push("--");
        status.extend([
            ":(top,exclude)Cargo.toml",
            ":(top,exclude)Cargo.lock",
            ":(top,exclude)client/macos/Info.plist",
        ]);
    }
    let dirty = git(&status).is_some_and(|s| !s.is_empty());

    let built_at = rfc3339_utc(build_epoch_seconds());
    let source = format!(
        "/// The commit of HEAD at build time: hex digits, or \"unknown\" without git.\n\
         pub const COMMIT: &str = {commit:?};\n\
         /// True when the worktree had uncommitted changes at build time.\n\
         pub const DIRTY: bool = {dirty};\n\
         /// The build time, RFC 3339 in UTC.\n\
         pub const BUILT_AT: &str = {built_at:?};\n"
    );
    let out_dir = std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR");
    std::fs::write(Path::new(&out_dir).join("stamp.rs"), source)
        .expect("write the build stamp to OUT_DIR");

    emit_rerun_triggers();
}

/// Stdout of `git <args>` (trimmed), or `None` if git is missing or fails.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `SOURCE_DATE_EPOCH` makes a build reproducible; otherwise the clock.
fn build_epoch_seconds() -> u64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        })
}

/// `2026-10-02T09:12:00Z` for a Unix time. No date crate: this crate has no
/// dependencies, and the civil-from-days algorithm is short.
fn rfc3339_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// Make cargo run this script again when the stamp could change.
///
/// Once a build script prints any `rerun-if-changed`, cargo stops its default
/// "rerun when a package file changes". So we list everything the stamp
/// depends on:
/// - HEAD, the index, the branch ref and `packed-refs`: a commit, checkout,
///   `git add` or `git pack-refs` changes the commit or the dirty flag;
/// - the source trees of the client and of `mello-core`: an edit changes the
///   dirty flag and the build time.
///
/// A linked worktree has a `.git` FILE that points to its own git dir. The
/// branch ref and `packed-refs` are in the shared (common) git dir.
fn emit_rerun_triggers() {
    // `--git-dir` and `--git-common-dir` print a path relative to the current
    // directory in a plain repo, so make every path absolute.
    let path_of = |args: &[&str]| {
        git(args).map(|p| {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir().unwrap_or_default().join(p)
            }
        })
    };
    if let (Some(git_dir), Some(common_dir)) = (
        path_of(&["rev-parse", "--git-dir"]),
        path_of(&["rev-parse", "--git-common-dir"]),
    ) {
        let mut watch = vec![
            git_dir.join("HEAD"),
            git_dir.join("index"),
            common_dir.join("packed-refs"),
        ];
        if let Some(branch_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watch.push(common_dir.join(branch_ref));
        }
        for path in watch {
            // A missing path makes cargo rerun on every build. A repo without
            // packed-refs, or a detached HEAD, must not cause that.
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }

    for path in [
        "build.rs",
        "../Cargo.toml",
        "../Cargo.lock",
        "../client/build.rs",
        "../client/Cargo.toml",
        "../client/src",
        "../client/ui",
        "../client/assets",
        "../mello-core/Cargo.toml",
        "../mello-core/src",
    ] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-env-changed=GITHUB_ACTIONS");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}
