//! The single-instance lock on Linux: an abstract Unix socket bound to the
//! instance name, the same name and namespace the `single-instance` crate
//! binds, so old and new versions still see each other.
//!
//! The crate binds without SOCK_CLOEXEC, so every child process inherited the
//! lock. Velopack's updater is such a child: it restarted the app after an
//! update while it still held the lock, and the new instance quit with
//! "Mello is already running" (2026-11-01). A browser or a game started from
//! the app kept it too. std's UnixListener sets close-on-exec.

use std::io::ErrorKind;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};

/// Held for the life of the process. `is_single()` is false when another
/// instance holds the name.
pub struct InstanceLock {
    listener: Option<UnixListener>,
}

impl InstanceLock {
    pub fn acquire(name: &str) -> std::io::Result<Self> {
        let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
        match UnixListener::bind_addr(&addr) {
            Ok(listener) => Ok(Self {
                listener: Some(listener),
            }),
            Err(e) if e.kind() == ErrorKind::AddrInUse => Ok(Self { listener: None }),
            Err(e) => Err(e),
        }
    }

    pub fn is_single(&self) -> bool {
        self.listener.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(name: &str) -> String {
        format!("m3llo-test-{name}-{}", std::process::id())
    }

    #[test]
    fn a_second_instance_is_refused() {
        let name = unique("second");
        let first = InstanceLock::acquire(&name).expect("first");
        assert!(first.is_single());
        let second = InstanceLock::acquire(&name).expect("second");
        assert!(!second.is_single());
        drop(first);
        assert!(InstanceLock::acquire(&name).expect("again").is_single());
    }

    /// The lock goes with the process, not with its children: a child that
    /// outlives the lock (the updater, a browser) does not hold it.
    #[test]
    fn a_child_process_does_not_keep_the_lock() {
        let name = unique("child");
        let lock = InstanceLock::acquire(&name).expect("lock");
        assert!(lock.is_single());
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .expect("sleep");
        drop(lock);
        let again = InstanceLock::acquire(&name).expect("again");
        let _ = child.kill();
        let _ = child.wait();
        assert!(again.is_single(), "the child kept the lock");
    }
}
