//! Cisco's OpenH264 binary for the Linux stream viewer (spec 12-STREAMING
//! section 7.4).
//!
//! Cisco pays the H.264 patent royalty for its own binary only, and only
//! when the binary is downloaded to the user's device by itself and the user
//! can turn it on and off (Cisco's BINARY_LICENSE.txt). So the AppImage does
//! not carry it. The client downloads it once, in the background, into the
//! app data directory. The "Software Video Decoder" setting turns it off.
//!
//! libmello reads two variables when it loads OpenH264
//! (`openh264_dynload_linux.cpp`): [`PATH_ENV`] names the downloaded file,
//! which it tries first, and [`DISABLED_ENV`]`=1` turns OpenH264 off.
//! [`prepare`] sets them at startup, before any other thread reads the
//! environment.
//!
//! A file is at the path only after it passed both SHA-256 checks: the
//! download goes to a temporary file in the same directory, then a rename.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// The file libmello loads first.
pub const PATH_ENV: &str = "MELLO_OPENH264_PATH";
/// `1`: libmello loads no OpenH264.
pub const DISABLED_ENV: &str = "MELLO_OPENH264_DISABLED";

/// One release of Cisco's binary. The sizes and hashes are pinned: Cisco
/// publishes only an MD5, so the client checks against values taken once
/// from the file (the MD5 matched Cisco's `.signed.md5.txt`).
#[derive(Debug, Clone, Copy)]
pub struct Binary {
    pub file_name: &'static str,
    pub url: &'static str,
    pub bz2_size: u64,
    pub bz2_sha256: &'static str,
    pub so_size: u64,
    pub so_sha256: &'static str,
}

/// OpenH264 2.6.0, soname 8: the ABI of the headers libmello is built with.
pub const CISCO: Binary = Binary {
    file_name: "libopenh264-2.6.0-linux64.8.so",
    url: "https://ciscobinary.openh264.org/libopenh264-2.6.0-linux64.8.so.bz2",
    bz2_size: 634_264,
    bz2_sha256: "27ab53323c110b76214c1c72222f459d17febbcd1e252136cadc292b0308d75b",
    so_size: 1_731_128,
    so_sha256: "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04",
};

/// Where a system OpenH264 lives on the distributions in the plan. The
/// dynamic loader finds `libopenh264.so.8` in these by its cache.
const SYSTEM_DIRS: [&str; 6] = [
    "/usr/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib",
    "/lib64",
    "/lib/x86_64-linux-gnu",
    "/usr/local/lib",
];
const SYSTEM_SONAME: &str = "libopenh264.so.8";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("download: {0}")]
    Download(String),
    #[error("{what} is {got} bytes, expected {want}")]
    Size {
        what: &'static str,
        got: u64,
        want: u64,
    },
    #[error("{what} has the wrong SHA-256")]
    Hash { what: &'static str },
    #[error("bzip2: {0}")]
    Bzip2(std::io::Error),
    #[error("file: {0}")]
    Io(#[from] std::io::Error),
}

/// What startup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    /// The user turned the codec off. Nothing loads, nothing downloads.
    Disabled,
    /// Our checked copy is on disk.
    Installed(PathBuf),
    /// The distribution has one. No download: no request to Cisco.
    System(PathBuf),
    /// Nothing on disk. Download in the background.
    Download,
}

/// The startup decision, from what is on disk.
pub fn decide(enabled: bool, installed: Option<PathBuf>, system: Option<PathBuf>) -> Startup {
    if !enabled {
        return Startup::Disabled;
    }
    if let Some(path) = installed {
        return Startup::Installed(path);
    }
    if let Some(path) = system {
        return Startup::System(path);
    }
    Startup::Download
}

/// Startup, on the main thread before any other thread reads the
/// environment: checks the disk, sets the variables for libmello, and
/// returns what it found. A [`Startup::Download`] is for [`spawn_download`].
pub fn prepare(enabled: bool, codec_dir: &Path) -> Startup {
    let startup = if enabled {
        decide(
            true,
            installed(codec_dir, &CISCO),
            system_library(&SYSTEM_DIRS),
        )
    } else {
        Startup::Disabled
    };
    match &startup {
        Startup::Disabled => std::env::set_var(DISABLED_ENV, "1"),
        // The path is set before the file exists: a download that finishes
        // later is loaded by the next decoder libmello makes.
        _ => std::env::set_var(PATH_ENV, codec_dir.join(CISCO.file_name)),
    }
    log::info!("[openh264] startup: {startup:?}");
    startup
}

/// Downloads and installs the binary on a background thread. A failure is
/// logged; the next start tries again.
pub fn spawn_download(codec_dir: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("openh264-download".into())
        .spawn(move || {
            log::info!("[openh264] downloading {}", CISCO.url);
            match download(&CISCO).and_then(|bz2| install(&codec_dir, &CISCO, &bz2)) {
                Ok(path) => log::info!("[openh264] installed {}", path.display()),
                Err(e) => log::warn!("[openh264] not installed: {e}"),
            }
        });
    if let Err(e) = spawned {
        log::warn!("[openh264] no download thread: {e}");
    }
}

/// Our copy in `dir`, if it is there and passes the check. A file that fails
/// it is removed, so the next download replaces it.
pub fn installed(dir: &Path, bin: &Binary) -> Option<PathBuf> {
    let path = dir.join(bin.file_name);
    let bytes = std::fs::read(&path).ok()?;
    if check(&bytes, bin.so_size, bin.so_sha256, "the library").is_ok() {
        return Some(path);
    }
    log::warn!("[openh264] {} fails the check, removing it", path.display());
    let _ = std::fs::remove_file(&path);
    None
}

/// The first system `libopenh264.so.8` in `dirs`.
pub fn system_library(dirs: &[&str]) -> Option<PathBuf> {
    dirs.iter()
        .map(|d| Path::new(d).join(SYSTEM_SONAME))
        .find(|p| p.is_file())
}

/// The `.bz2` from Cisco, at most one byte more than the pinned size.
pub fn download(bin: &Binary) -> Result<Vec<u8>, Error> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| Error::Download(e.to_string()))?;
    let response = client
        .get(bin.url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::Download(e.to_string()))?;
    let mut body = Vec::new();
    response.take(bin.bz2_size + 1).read_to_end(&mut body)?;
    Ok(body)
}

/// Checks the `.bz2`, unpacks it, checks the library, and moves it into
/// `dir`. Returns the path of the installed file.
pub fn install(dir: &Path, bin: &Binary, bz2: &[u8]) -> Result<PathBuf, Error> {
    check(bz2, bin.bz2_size, bin.bz2_sha256, "the download")?;
    let mut so = Vec::new();
    bzip2::read::BzDecoder::new(bz2)
        .take(bin.so_size + 1)
        .read_to_end(&mut so)
        .map_err(Error::Bzip2)?;
    check(&so, bin.so_size, bin.so_sha256, "the library")?;

    std::fs::create_dir_all(dir)?;
    let path = dir.join(bin.file_name);
    let part = dir.join(format!(".{}.part", bin.file_name));
    let written = (|| {
        let mut file = std::fs::File::create(&part)?;
        file.write_all(&so)?;
        file.sync_all()?;
        std::fs::rename(&part, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&part);
        return Err(Error::Io(e));
    }
    Ok(path)
}

fn check(bytes: &[u8], size: u64, sha256: &str, what: &'static str) -> Result<(), Error> {
    let got = bytes.len() as u64;
    if got != size {
        return Err(Error::Size {
            what,
            got,
            want: size,
        });
    }
    if hex(&Sha256::digest(bytes)) != sha256 {
        return Err(Error::Hash { what });
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for Cisco's release: some bytes, their `.bz2`, and a
    /// [`Binary`] with their sizes and hashes.
    fn fake_release(content: &[u8]) -> (Binary, Vec<u8>) {
        let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::best());
        enc.write_all(content).unwrap();
        let bz2 = enc.finish().unwrap();
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let bin = Binary {
            file_name: "libopenh264-test.so",
            url: "https://example.invalid/libopenh264-test.so.bz2",
            bz2_size: bz2.len() as u64,
            bz2_sha256: leak(hex(&Sha256::digest(&bz2))),
            so_size: content.len() as u64,
            so_sha256: leak(hex(&Sha256::digest(content))),
        };
        (bin, bz2)
    }

    /// A directory of its own under the system temp directory, removed on
    /// drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("m3llo-openh264-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn library_bytes() -> Vec<u8> {
        (0..64 * 1024u32).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn a_checked_download_lands_at_the_path_libmello_loads() {
        let dir = tempdir();
        let content = library_bytes();
        let (bin, bz2) = fake_release(&content);
        let path = install(dir.path(), &bin, &bz2).unwrap();
        assert_eq!(path, dir.path().join(bin.file_name));
        assert_eq!(std::fs::read(&path).unwrap(), content);
        assert_eq!(files_in(dir.path()), vec![bin.file_name.to_string()]);
        assert_eq!(installed(dir.path(), &bin), Some(path));
    }

    #[test]
    fn a_download_with_the_wrong_hash_leaves_no_file() {
        let dir = tempdir();
        let (bin, mut bz2) = fake_release(&library_bytes());
        let last = bz2.len() - 1;
        bz2[last] ^= 0xff;
        let err = install(dir.path(), &bin, &bz2).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Hash {
                    what: "the download"
                }
            ),
            "{err}"
        );
        assert!(files_in(dir.path()).is_empty());
    }

    #[test]
    fn a_download_of_another_size_leaves_no_file() {
        let dir = tempdir();
        let (bin, bz2) = fake_release(&library_bytes());
        let err = install(dir.path(), &bin, &bz2[..bz2.len() - 1]).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Size {
                    what: "the download",
                    ..
                }
            ),
            "{err}"
        );
        assert!(files_in(dir.path()).is_empty());
    }

    #[test]
    fn a_library_that_unpacks_wrong_leaves_no_file() {
        // The .bz2 passes its check, the library in it does not: the pinned
        // library hash belongs to other bytes.
        let dir = tempdir();
        let (mut bin, bz2) = fake_release(&library_bytes());
        let (other, _) = fake_release(b"other bytes of the same kind");
        bin.so_sha256 = other.so_sha256;
        let err = install(dir.path(), &bin, &bz2).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Hash {
                    what: "the library"
                }
            ),
            "{err}"
        );
        assert!(files_in(dir.path()).is_empty());
    }

    #[test]
    fn a_damaged_copy_on_disk_is_removed() {
        let dir = tempdir();
        let (bin, _) = fake_release(&library_bytes());
        std::fs::write(dir.path().join(bin.file_name), b"damaged").unwrap();
        assert_eq!(installed(dir.path(), &bin), None);
        assert!(files_in(dir.path()).is_empty());
    }

    #[test]
    fn no_copy_on_disk_is_not_installed() {
        let dir = tempdir();
        assert_eq!(installed(dir.path(), &CISCO), None);
    }

    #[test]
    fn the_system_library_is_found_in_the_first_dir_that_has_it() {
        let a = tempdir();
        let b = tempdir();
        std::fs::write(b.path().join(SYSTEM_SONAME), b"lib").unwrap();
        let dirs = [a.path().to_str().unwrap(), b.path().to_str().unwrap()];
        assert_eq!(system_library(&dirs), Some(b.path().join(SYSTEM_SONAME)));
        assert_eq!(system_library(&dirs[..1]), None);
    }

    #[test]
    fn startup_downloads_only_when_on_and_nothing_is_on_disk() {
        let ours = PathBuf::from("/data/codecs/libopenh264-2.6.0-linux64.8.so");
        let system = PathBuf::from("/usr/lib/libopenh264.so.8");
        assert_eq!(decide(false, None, None), Startup::Disabled);
        assert_eq!(
            decide(false, Some(ours.clone()), Some(system.clone())),
            Startup::Disabled
        );
        assert_eq!(
            decide(true, Some(ours.clone()), Some(system.clone())),
            Startup::Installed(ours)
        );
        assert_eq!(
            decide(true, None, Some(system.clone())),
            Startup::System(system)
        );
        assert_eq!(decide(true, None, None), Startup::Download);
    }

    #[test]
    fn the_pinned_release_is_the_one_libmello_loads_first() {
        // openh264_dynload_linux.cpp names this file next to the executable,
        // and its headers are v2.6.0 with soname 8.
        assert_eq!(CISCO.file_name, "libopenh264-2.6.0-linux64.8.so");
        assert!(CISCO.url.starts_with("https://ciscobinary.openh264.org/"));
        assert!(CISCO.url.ends_with(&format!("{}.bz2", CISCO.file_name)));
        assert_eq!(CISCO.bz2_sha256.len(), 64);
        assert_eq!(CISCO.so_sha256.len(), 64);
    }
}
