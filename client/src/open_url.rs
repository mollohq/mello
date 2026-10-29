//! Open a web address in the system browser. One path for every link the UI
//! opens: a link in chat, a tester's Steam profile.
//!
//! In a test the address is recorded and no browser starts, so a test reads
//! what the click asked for with [`take_opened`].

#[cfg(not(test))]
pub fn open(url: &str) {
    if let Err(e) = open::that(url) {
        log::warn!("Failed to open URL {}: {}", url, e);
    }
}

#[cfg(test)]
thread_local! {
    static OPENED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub fn open(url: &str) {
    OPENED.with(|o| o.borrow_mut().push(url.to_string()));
}

/// The addresses opened on this thread since the last call.
#[cfg(test)]
pub fn take_opened() -> Vec<String> {
    OPENED.with(|o| std::mem::take(&mut *o.borrow_mut()))
}
