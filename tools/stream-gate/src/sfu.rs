//! The real SFU in test mode, one process for the whole gate run.
//!
//! The harness builds `./cmd/sfu` from `MELLO_SFU_DIR` (default: the
//! `mello-sfu` checkout next to this repository) with `go build` into the
//! output directory and runs the binary. `go run` builds the same binary, but
//! on Windows it leaves the program running when the `go` process is killed,
//! with UDP port 10000 still bound; a binary the harness owns stops with it.
//!
//! Environment of the SFU process:
//!
//! | Variable | Value | Why |
//! |---|---|---|
//! | `SFU_ADMIN_PASSWORD` | random per run | test-mode join (01-SFU.md section 5.1) |
//! | `SFU_PUBLIC_IP` | `127.0.0.2` | ICE-lite and the UDP mux on `0.0.0.0:10000`; the one candidate is the shim |
//! | `SFU_ICE_TCP_PORT` | `0` | no ICE-TCP candidate: UDP only, through the shim |
//! | `SFU_ADDR`, `SFU_ADMIN_ADDR` | free loopback ports | signaling and `/health` |
//! | metering, R2, JWT, TLS, peers | removed | nothing leaves the machine |

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const SHIM_IP: &str = "127.0.0.2";
pub const MUX_PORT: u16 = 10000;

pub struct Sfu {
    child: Child,
    pub ws: String,
    pub admin: SocketAddr,
    pub password: String,
    pub commit: String,
    pub toolchain: String,
    pub dir: PathBuf,
    pub log: PathBuf,
}

fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    Ok(l.local_addr().map_err(|e| e.to_string())?.port())
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `GET /health` answers 200.
fn healthy(admin: SocketAddr) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&admin, Duration::from_millis(300)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    if s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 32];
    matches!(s.read(&mut buf), Ok(n) if n >= 12 && &buf[9..12] == b"200")
}

impl Sfu {
    pub fn start(dir: &Path, out_dir: &Path) -> Result<Self, String> {
        if !dir.join("cmd/sfu").is_dir() {
            return Err(format!(
                "{}: no cmd/sfu. Set MELLO_SFU_DIR to a mello-sfu checkout.",
                dir.display()
            ));
        }
        let exe = out_dir.join(if cfg!(windows) { "sfu.exe" } else { "sfu" });
        let toolchain =
            std::env::var("STREAM_GATE_GOTOOLCHAIN").unwrap_or_else(|_| "go1.24.9".into());
        let built = Command::new("go")
            .env("GOTOOLCHAIN", &toolchain)
            .arg("build")
            .arg("-o")
            .arg(&exe)
            .arg("./cmd/sfu")
            .current_dir(dir)
            .status()
            .map_err(|e| format!("go build: {e} (is Go installed?)"))?;
        if !built.success() {
            return Err(format!("go build ./cmd/sfu failed in {}", dir.display()));
        }
        let password = format!(
            "gate{:016x}",
            crate::clip::fnv1a(
                crate::clip::FNV_INIT,
                format!("{:?}{}", Instant::now(), std::process::id()).as_bytes()
            )
        );
        let (ws_port, admin_port) = (free_port()?, free_port()?);
        let log = out_dir.join("sfu.log");
        let log_file =
            std::fs::File::create(&log).map_err(|e| format!("{}: {e}", log.display()))?;
        let err_file = log_file.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&exe);
        for k in [
            "SFU_METERING_SECRET",
            "SFU_METERING_URL",
            "SFU_R2_ENDPOINT",
            "SFU_R2_ACCESS_KEY_ID",
            "SFU_R2_SECRET_ACCESS_KEY",
            "SFU_JWT_PUB_KEY",
            "SFU_CERT_FILE",
            "SFU_KEY_FILE",
            "SFU_PEERS",
            "SFU_STATS_TOKEN",
            "SFU_ADMIN_IPS",
        ] {
            cmd.env_remove(k);
        }
        cmd.env("SFU_ADMIN_PASSWORD", &password)
            .env("SFU_PUBLIC_IP", SHIM_IP)
            .env("SFU_ICE_TCP_PORT", "0")
            .env("SFU_ADDR", format!("127.0.0.1:{ws_port}"))
            .env("SFU_ADMIN_ADDR", format!("127.0.0.1:{admin_port}"))
            .env("SFU_REGION", "gate")
            .env("SFU_SERVER_ID", "sfu-gate")
            .env("SFU_LOG_LEVEL", "info")
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(err_file));
        let child = cmd.spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
        let admin: SocketAddr = format!("127.0.0.1:{admin_port}")
            .parse()
            .map_err(|e| format!("{e}"))?;
        let mut sfu = Self {
            child,
            ws: format!("ws://127.0.0.1:{ws_port}/ws"),
            admin,
            password,
            commit: git(dir, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into()),
            toolchain,
            dir: dir.to_owned(),
            log,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = sfu.child.try_wait() {
                return Err(format!(
                    "the SFU exited at start ({status}); see {}. Is UDP port {MUX_PORT} in use (a local SFU)?",
                    sfu.log.display()
                ));
            }
            if healthy(sfu.admin) {
                return Ok(sfu);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "the SFU did not answer /health in 20 s; see {}",
            sfu.log.display()
        ))
    }

    /// The process handle, for the load guard (`load.rs`).
    pub fn raw_handle(&self) -> *mut std::ffi::c_void {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            self.child.as_raw_handle()
        }
        #[cfg(not(windows))]
        {
            std::ptr::null_mut()
        }
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Sfu {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
