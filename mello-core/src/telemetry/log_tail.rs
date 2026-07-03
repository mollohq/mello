//! Shared log tailer for file-based telemetry adapters (spec 18 §2.1,
//! "log/file tailer" source class).
//!
//! Follows a live log file the way game trackers do: start at the current end
//! (history is not replayed), poll for growth, feed complete lines to the
//! adapter's callback, and reopen from the start when the file shrinks or is
//! rotated. The watched path is re-resolved while the file doesn't exist yet
//! (games create logs lazily, sometimes in per-session directories).

use std::io::{BufRead, Seek};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const POLL_INTERVAL_MS: u64 = 500;

/// Spawn a tail worker. `resolve` locates the current log file (called until
/// one exists), `on_line` receives each complete new line. The worker stops
/// when `running` goes false.
pub(crate) fn spawn_tail(
    thread_name: &str,
    running: Arc<AtomicBool>,
    resolve: impl Fn() -> Option<PathBuf> + Send + 'static,
    mut on_line: impl FnMut(&str) + Send + 'static,
) {
    let name = thread_name.to_string();
    let spawn = std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || tail_loop(&running, &resolve, &mut on_line));
    if let Err(e) = spawn {
        log::warn!("[telemetry] {name} tail thread failed to spawn: {e}");
    }
}

fn tail_loop(
    running: &AtomicBool,
    resolve: &dyn Fn() -> Option<PathBuf>,
    on_line: &mut dyn FnMut(&str),
) {
    let mut current: Option<(PathBuf, std::io::BufReader<std::fs::File>, u64)> = None;

    while running.load(Ordering::SeqCst) {
        // (Re)open when we have no file yet, or when the path resolution moved
        // (per-session log directories) or the file shrank (rotation).
        let resolved = resolve();
        match (&mut current, resolved) {
            (slot @ None, Some(path)) => {
                if let Ok(file) = std::fs::File::open(&path) {
                    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
                    let mut reader = std::io::BufReader::new(file);
                    // Start at EOF: only lines written after we attach count.
                    let _ = reader.seek(std::io::SeekFrom::End(0));
                    *slot = Some((path, reader, len));
                }
            }
            (Some((path, _, pos)), Some(new_path)) => {
                let moved = *path != new_path;
                let shrank = std::fs::metadata(path)
                    .map(|m| m.len() < *pos)
                    .unwrap_or(true);
                if moved || shrank {
                    // Rotated or replaced: reopen from the start so nothing in
                    // the fresh file is missed.
                    if let Ok(file) = std::fs::File::open(&new_path) {
                        current = Some((new_path, std::io::BufReader::new(file), 0));
                    } else {
                        current = None;
                    }
                }
            }
            _ => {}
        }

        if let Some((_, reader, pos)) = &mut current {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        *pos += n as u64;
                        // Incomplete trailing line (no newline yet): rewind so
                        // it's re-read whole on the next pass.
                        if !line.ends_with('\n') {
                            let _ = reader.seek_relative(-(n as i64));
                            *pos -= n as u64;
                            break;
                        }
                        on_line(line.trim_end());
                    }
                }
                if !running.load(Ordering::SeqCst) {
                    return;
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;

    #[test]
    fn tails_only_new_lines_and_stops() {
        let dir = std::env::temp_dir().join(format!("mello-tail-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.log");
        std::fs::write(&path, "old line\n").unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::channel::<String>();
        let resolve_path = path.clone();
        spawn_tail(
            "test-tail",
            running.clone(),
            move || Some(resolve_path.clone()),
            move |line| {
                let _ = tx.send(line.to_string());
            },
        );

        // Give the tailer time to attach at EOF, then append.
        std::thread::sleep(std::time::Duration::from_millis(800));
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "new line 1").unwrap();
            writeln!(f, "new line 2").unwrap();
        }

        let first = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let second = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(first, "new line 1");
        assert_eq!(second, "new line 2");

        running.store(false, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
