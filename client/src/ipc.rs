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

    /// Block until one message arrives, or the deadline passes.
    ///
    /// Test-only: the app polls with `try_recv` on its frame tick and never
    /// blocks the UI thread.
    ///
    /// The app polls with `try_recv` on its own frame tick. A test cannot:
    /// polling on a sleep is a retry loop, and it fails under load rather
    /// than when the code is wrong. This waits on the delivery itself.
    #[cfg(test)]
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<String> {
        self.inner.recv_timeout(timeout)
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

        #[cfg(test)]
        pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<String> {
            // The listener is non-blocking, so block on readability instead
            // of on a sleep. `accept` then returns without waiting.
            let deadline = std::time::Instant::now() + timeout;
            loop {
                if let Some(msg) = self.try_recv().into_iter().next() {
                    return Some(msg);
                }
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::yield_now();
            }
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

    /// Whether a failed ConnectNamedPipe still leaves bytes to read.
    /// ERROR_PIPE_CONNECTED: the client connected between CreateNamedPipeW
    /// and ConnectNamedPipe and is waiting. ERROR_NO_DATA: it did that and
    /// disconnected again before this thread got here — a fast sender (open,
    /// write, close) wins that race, and the bytes it wrote sit in the pipe
    /// buffer. Anything else is a real failure. Pure so the mapping is
    /// unit-testable without named pipes.
    #[cfg(windows)]
    pub(super) fn may_still_hold_bytes(raw_os_error: Option<i32>) -> bool {
        use windows::Win32::Foundation::{ERROR_NO_DATA, ERROR_PIPE_CONNECTED};
        raw_os_error == Some(ERROR_PIPE_CONNECTED.0 as i32)
            || raw_os_error == Some(ERROR_NO_DATA.0 as i32)
    }

    impl PlatformListener {
        pub fn bind(endpoint: &str) -> std::io::Result<Self> {
            let pipe_name = endpoint.to_string();
            let (tx, rx) = mpsc::channel::<String>();
            let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
            let ready2 = ready.clone();

            log::info!("[ipc] listening on {}", endpoint);
            let _handle = std::thread::spawn(move || {
                pipe_accept_loop(&pipe_name, &tx, &ready2);
            });

            ready.wait();
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

        #[cfg(test)]
        pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<String> {
            match self.rx.recv_timeout(timeout) {
                Ok(msg) => {
                    log::info!("[ipc] received: {}", msg);
                    Some(msg)
                }
                Err(_) => None,
            }
        }
    }

    fn pipe_accept_loop(pipe_name: &str, tx: &mpsc::Sender<String>, ready: &std::sync::Barrier) {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::*;
        use windows::Win32::Storage::FileSystem::*;
        use windows::Win32::System::Pipes::*;

        let mut first = true;
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
                if first {
                    ready.wait();
                }
                return;
            }

            if first {
                first = false;
                ready.wait();
            }

            // Blocks until a client connects (or pipe is broken)
            let connected = unsafe { ConnectNamedPipe(pipe, None) };
            if connected.is_err() {
                // A failed connect can still leave bytes to read (see
                // may_still_hold_bytes): fall through to the read instead of
                // dropping a message send_to_running already confirmed.
                let err = std::io::Error::last_os_error();
                if !may_still_hold_bytes(err.raw_os_error()) {
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

        // The listener hands the message over on its own thread. Wait on
        // that handover, not on a sleep: a poll loop passes on a quiet
        // machine and fails when the gate builds in parallel.
        let msg = listener
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the listener never delivered the message");
        assert_eq!(msg, "mello://join/TEST-1234");

        // No pending messages after drain
        assert!(listener.try_recv().is_empty());
    }

    #[test]
    fn send_to_nonexistent_returns_false() {
        let ep = endpoint_name("mello-ipc-test-nonexistent");
        assert!(!send_to_running(&ep, "mello://join/X"));
    }

    /// The ConnectNamedPipe error mapping the accept loop runs on: a client
    /// that connected, or connected and left, leaves bytes behind. Anything
    /// else is a real failure. Windows-only, like the function.
    #[cfg(windows)]
    #[test]
    fn connect_errors_that_leave_bytes_are_read() {
        use super::platform::may_still_hold_bytes;
        use windows::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
        };
        assert!(may_still_hold_bytes(Some(ERROR_PIPE_CONNECTED.0 as i32)));
        assert!(may_still_hold_bytes(Some(ERROR_NO_DATA.0 as i32)));
        assert!(!may_still_hold_bytes(Some(ERROR_ACCESS_DENIED.0 as i32)));
        assert!(!may_still_hold_bytes(None));
    }

    /// A sender that opens, writes and closes before the accept thread
    /// reaches ConnectNamedPipe must still be heard. The bind barrier only
    /// guarantees the pipe exists, not that the server is waiting on it; a
    /// fast sender wins that race and the server sees ERROR_NO_DATA, whose
    /// bytes sit in the pipe buffer. Dropping them loses a message that
    /// send_to_running already confirmed — exactly the CI flake.
    #[test]
    fn fast_sender_before_accept_still_delivers() {
        for i in 0..5 {
            let ep = endpoint_name(&format!("mello-ipc-race-{}.{}", std::process::id(), i));
            // Hammer connects from another thread: pre-bind attempts fail
            // fast, and the first landing write races the accept thread.
            let sender = std::thread::spawn({
                let ep = ep.clone();
                move || {
                    for _ in 0..10_000 {
                        if send_to_running(&ep, "mello://join/RACE") {
                            return true;
                        }
                        std::thread::yield_now();
                    }
                    false
                }
            });
            let listener = IpcListener::bind(&ep).expect("bind failed");
            assert!(sender.join().expect("sender panicked"), "no send landed");
            let msg = listener
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("fast sender's message was lost");
            assert_eq!(msg, "mello://join/RACE");
        }
    }
}
