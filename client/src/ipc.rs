use std::io::{BufRead, BufReader, Write};

/// Derive the platform-specific IPC endpoint name from the app lock name.
pub fn endpoint_name(lock_name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!(r"\\.\pipe\{}", lock_name)
    } else {
        std::env::temp_dir()
            .join(format!("{}.sock", lock_name))
            .to_string_lossy()
            .to_string()
    }
}

// ── Listener (first instance) ─────────────────────────────────────────────

pub struct IpcListener {
    inner: PlatformListener,
}

impl IpcListener {
    pub fn bind(endpoint: &str) -> std::io::Result<Self> {
        Ok(Self {
            inner: PlatformListener::bind(endpoint)?,
        })
    }

    /// Non-blocking: returns any messages received since last call.
    pub fn try_recv(&self) -> Vec<String> {
        self.inner.try_recv()
    }
}

/// Send a message to the running instance and return true on success.
pub fn send_to_running(endpoint: &str, message: &str) -> bool {
    platform_send(endpoint, message)
}

// ── Unix implementation (macOS / Linux) ───────────────────────────────────

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    pub struct PlatformListener {
        listener: UnixListener,
        path: PathBuf,
    }

    impl PlatformListener {
        pub fn bind(endpoint: &str) -> std::io::Result<Self> {
            let path = PathBuf::from(endpoint);
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)?;
            listener.set_nonblocking(true)?;
            log::info!("[ipc] listening on {}", endpoint);
            Ok(Self { listener, path })
        }

        pub fn try_recv(&self) -> Vec<String> {
            let mut messages = Vec::new();
            loop {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
                            .ok();
                        if let Some(msg) = read_line(stream) {
                            log::info!("[ipc] received: {}", msg);
                            messages.push(msg);
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log::warn!("[ipc] accept error: {}", e);
                        break;
                    }
                }
            }
            messages
        }
    }

    impl Drop for PlatformListener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn read_line(stream: UnixStream) -> Option<String> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    pub fn platform_send(endpoint: &str, message: &str) -> bool {
        match UnixStream::connect(endpoint) {
            Ok(mut stream) => {
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(500)));
                if writeln!(stream, "{}", message).is_ok() {
                    log::info!("[ipc] sent to running instance: {}", message);
                    true
                } else {
                    log::warn!("[ipc] failed to write to socket");
                    false
                }
            }
            Err(e) => {
                log::warn!("[ipc] could not connect to running instance: {}", e);
                false
            }
        }
    }
}

// ── Windows implementation (Named Pipe) ───────────────────────────────────

#[cfg(windows)]
mod platform {
    use super::*;
    use std::sync::mpsc;

    pub struct PlatformListener {
        rx: mpsc::Receiver<String>,
        _handle: std::thread::JoinHandle<()>,
    }

    impl PlatformListener {
        pub fn bind(endpoint: &str) -> std::io::Result<Self> {
            let pipe_name = endpoint.to_string();
            let (tx, rx) = mpsc::channel::<String>();

            log::info!("[ipc] listening on {}", endpoint);
            let _handle = std::thread::spawn(move || {
                pipe_accept_loop(&pipe_name, &tx);
            });

            Ok(Self { rx, _handle })
        }

        pub fn try_recv(&self) -> Vec<String> {
            let mut messages = Vec::new();
            while let Ok(msg) = self.rx.try_recv() {
                log::info!("[ipc] received: {}", msg);
                messages.push(msg);
            }
            messages
        }
    }

    fn pipe_accept_loop(pipe_name: &str, tx: &mpsc::Sender<String>) {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::*;
        use windows::Win32::Storage::FileSystem::*;
        use windows::Win32::System::Pipes::*;

        loop {
            let h_pipe_name = HSTRING::from(pipe_name);
            let pipe = unsafe {
                CreateNamedPipeW(
                    &h_pipe_name,
                    PIPE_ACCESS_INBOUND,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    0,
                    1024,
                    0,
                    None,
                )
            };
            if pipe == INVALID_HANDLE_VALUE {
                log::error!("[ipc] CreateNamedPipeW failed, stopping listener");
                return;
            }

            // Blocks until a client connects (or pipe is broken)
            let connected = unsafe { ConnectNamedPipe(pipe, None) };
            if connected.is_err() {
                // ERROR_PIPE_CONNECTED means client connected before we called ConnectNamedPipe
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(ERROR_PIPE_CONNECTED.0 as i32) {
                    log::warn!("[ipc] ConnectNamedPipe error: {}", err);
                    unsafe {
                        let _ = CloseHandle(pipe);
                    }
                    continue;
                }
            }

            let file = unsafe { std::fs::File::from_raw_handle(pipe.0 as *mut _) };
            let mut reader = BufReader::new(file);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() && tx.send(trimmed).is_err() {
                    return; // receiver dropped, main app shutting down
                }
            }
            // pipe handle closed when `file` drops
        }
    }

    #[cfg(windows)]
    use std::os::windows::io::FromRawHandle;

    pub fn platform_send(endpoint: &str, message: &str) -> bool {
        use std::fs::OpenOptions;
        match OpenOptions::new().write(true).open(endpoint) {
            Ok(mut file) => {
                if writeln!(file, "{}", message).is_ok() {
                    log::info!("[ipc] sent to running instance: {}", message);
                    true
                } else {
                    log::warn!("[ipc] failed to write to pipe");
                    false
                }
            }
            Err(e) => {
                log::warn!("[ipc] could not connect to running instance: {}", e);
                false
            }
        }
    }
}

#[cfg(unix)]
use platform::platform_send;
#[cfg(unix)]
use platform::PlatformListener;

#[cfg(windows)]
use platform::platform_send;
#[cfg(windows)]
use platform::PlatformListener;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_name_unix() {
        if !cfg!(unix) {
            return;
        }
        let name = endpoint_name("app.mello.desktop");
        assert!(name.ends_with("app.mello.desktop.sock"));
    }

    #[test]
    fn round_trip() {
        let ep = endpoint_name(&format!("mello-ipc-test.{}", std::process::id()));
        let listener = IpcListener::bind(&ep).expect("bind failed");

        assert!(send_to_running(&ep, "mello://join/TEST-1234"));

        let msgs = listener.try_recv();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "mello://join/TEST-1234");

        // No pending messages after drain
        assert!(listener.try_recv().is_empty());
    }

    #[test]
    fn send_to_nonexistent_returns_false() {
        let ep = endpoint_name("mello-ipc-test-nonexistent");
        assert!(!send_to_running(&ep, "mello://join/X"));
    }
}
