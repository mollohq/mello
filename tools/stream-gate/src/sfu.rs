//! The real SFU in test mode, one process for the whole gate run.
//!
//! The harness builds `./cmd/sfu` from `MELLO_SFU_DIR` (default: the
//! `mello-sfu` checkout next to this repository) with `go build` into the
//! output directory and runs the binary. `go run` builds the same binary, but
//! on Windows it leaves the program running when the `go` process is killed,
//! with UDP port 10000 still bound; a binary the harness owns stops with it.
//!
//! The SFU stops with the harness in every exit path: `Drop` kills it, and on
//! Windows it runs in a job object that kills it when the harness process
//! ends, a crash included (a crashed harness left its SFU holding port 10000
//! and every later run failed at start). Before the start the harness names
//! the process that holds `0.0.0.0:10000`, if one does.
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
    /// The job object that kills the SFU when the harness ends (Windows).
    #[allow(dead_code)]
    job: KillOnCloseJob,
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
        check_mux_port_free()?;
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
        let job = KillOnCloseJob::assign(&child);
        let admin: SocketAddr = format!("127.0.0.1:{admin_port}")
            .parse()
            .map_err(|e| format!("{e}"))?;
        let mut sfu = Self {
            child,
            job,
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

/// The SFU binds `0.0.0.0:10000`, so no other process may hold that address
/// when it starts (the shim of this run holds `127.0.0.2:10000`, which does
/// not count). The error names the process that holds it, and says when it
/// is the SFU of an earlier gate run (a `sfu.exe` under a
/// `target/stream-gate` directory).
fn check_mux_port_free() -> Result<(), String> {
    match mux_port_holder() {
        None => Ok(()),
        Some((pid, path)) if is_gate_sfu(&path) => Err(format!(
            "UDP port {MUX_PORT} is held by the SFU of an earlier gate run that did not stop: \
             PID {pid} ({path}). Stop it (PowerShell: Stop-Process -Id {pid}) and run again."
        )),
        Some((pid, path)) => Err(format!(
            "UDP port {MUX_PORT} is held by PID {pid} ({path}): a local SFU? Stop it and run again."
        )),
    }
}

/// True for the image of a gate SFU: `sfu.exe` (or `sfu`) under a
/// `target/stream-gate` directory.
fn is_gate_sfu(path: &str) -> bool {
    let p = path.replace('\\', "/").to_ascii_lowercase();
    p.contains("/target/stream-gate/") && (p.ends_with("/sfu.exe") || p.ends_with("/sfu"))
}

/// The PID and image path of another process that holds `0.0.0.0:10000`
/// (Windows; elsewhere the SFU's own start error stays the report).
fn mux_port_holder() -> Option<(u32, String)> {
    #[cfg(windows)]
    {
        let own = std::process::id();
        let script = format!(
            "$e = Get-NetUDPEndpoint -LocalPort {MUX_PORT} -ErrorAction SilentlyContinue | \
             Where-Object {{ $_.LocalAddress -eq '0.0.0.0' -and $_.OwningProcess -ne {own} }} | \
             Select-Object -First 1; \
             if ($e) {{ $p = Get-Process -Id $e.OwningProcess -ErrorAction SilentlyContinue; \
             Write-Output (\"{{0}}|{{1}}\" -f $e.OwningProcess, $p.Path) }}"
        );
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let (pid, path) = text.trim().split_once('|')?;
        Some((pid.trim().parse().ok()?, path.trim().to_owned()))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// A Windows job object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: the SFU in
/// it ends when the last handle closes, so when the harness process ends in
/// any way. A no-op elsewhere (and when the job cannot be made: `Drop` still
/// kills the SFU on a normal exit).
struct KillOnCloseJob {
    #[cfg(windows)]
    handle: *mut std::ffi::c_void,
}

#[cfg(windows)]
mod job_ffi {
    use std::ffi::c_void;

    pub const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    pub const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    #[repr(C)]
    #[derive(Default)]
    pub struct IoCounters {
        pub counts: [u64; 6],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct BasicLimitInformation {
        pub per_process_user_time_limit: i64,
        pub per_job_user_time_limit: i64,
        pub limit_flags: u32,
        pub minimum_working_set_size: usize,
        pub maximum_working_set_size: usize,
        pub active_process_limit: u32,
        pub affinity: usize,
        pub priority_class: u32,
        pub scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct ExtendedLimitInformation {
        pub basic: BasicLimitInformation,
        pub io: IoCounters,
        pub process_memory_limit: usize,
        pub job_memory_limit: usize,
        pub peak_process_memory_used: usize,
        pub peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
        pub fn SetInformationJobObject(
            job: *mut c_void,
            class: i32,
            info: *mut c_void,
            length: u32,
        ) -> i32;
        pub fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        pub fn CloseHandle(handle: *mut c_void) -> i32;
    }
}

impl KillOnCloseJob {
    #[cfg(windows)]
    fn assign(child: &Child) -> Self {
        use job_ffi::*;
        use std::os::windows::io::AsRawHandle;
        // SAFETY: plain Win32 calls on a handle this function owns and the
        // child's process handle, which lives as long as `child`.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if handle.is_null() {
                log::warn!("gate: no job object for the SFU; it stops on a normal exit only");
                return Self { handle };
            }
            let mut info = ExtendedLimitInformation::default();
            info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                handle,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &mut info as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
            ) != 0
                && AssignProcessToJobObject(handle, child.as_raw_handle()) != 0;
            if !ok {
                log::warn!(
                    "gate: the SFU is not in a kill-on-close job; it stops on a normal exit only"
                );
            }
            Self { handle }
        }
    }

    #[cfg(not(windows))]
    fn assign(_child: &Child) -> Self {
        Self {}
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        #[cfg(windows)]
        if !self.handle.is_null() {
            // SAFETY: the handle came from CreateJobObjectW and is closed once.
            unsafe { job_ffi::CloseHandle(self.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gate_sfu_is_an_sfu_exe_under_target_stream_gate() {
        assert!(is_gate_sfu(
            r"C:\Users\bob\dev\m3llo-dev\mello-stream-4\target\stream-gate\cpu\sfu.exe"
        ));
        assert!(is_gate_sfu("/home/bob/mello/target/stream-gate/hw/sfu"));
        assert!(!is_gate_sfu(r"C:\Program Files\mello-sfu\sfu.exe"));
        assert!(!is_gate_sfu(
            r"C:\dev\target\stream-gate\hw\stream-gate.exe"
        ));
    }

    /// The job kills its process when its handle closes: what happens when
    /// the harness crashes.
    #[cfg(windows)]
    #[test]
    fn closing_the_job_kills_the_child() {
        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1 > NUL"])
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn");
        let job = KillOnCloseJob::assign(&child);
        assert!(child.try_wait().expect("wait").is_none());
        drop(job);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().expect("wait").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        panic!("the child outlived the job");
    }
}
