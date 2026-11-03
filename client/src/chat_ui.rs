//! Chat scroll / unread state shared between Slint and the event handlers.

use std::cell::{Cell, RefCell};

use crate::MainWindow;

/// Client-side chat scroll and unread boundary state.
#[derive(Debug, Default)]
pub struct ChatScrollState {
    pub at_bottom: Cell<bool>,
    pub has_new_messages: Cell<bool>,
    pub first_unread_id: RefCell<Option<String>>,
    pub pending_scroll_to_bottom: Cell<bool>,
    /// Rows prepended on the last history load (for the viewport nudge).
    pub history_prepended_rows: Cell<i32>,
    /// How many of the newest loaded messages the feed shows. The feed is a
    /// plain Flickable, so this bounds its rows. It grows while the user reads
    /// older history and goes back to `WINDOW` when they return to the bottom.
    pub window_len: Cell<usize>,
    /// The server has pages older than the oldest loaded message.
    pub server_has_more: Cell<bool>,
    /// Open the feed at the unread rule instead of the bottom.
    pub pending_scroll_to_unread: Cell<bool>,
    /// The last (crew, message) read cursor sent, so a bottom reached twice
    /// writes once.
    pub last_marked: RefCell<Option<(String, String)>>,
}

impl ChatScrollState {
    /// Rows the feed shows when the user is at the bottom.
    pub const WINDOW: usize = 200;
    /// Rows revealed from the already-loaded messages per scroll to the top,
    /// before any page is asked from the server.
    pub const LOCAL_PAGE: usize = 50;

    pub fn new() -> Self {
        let s = Self::default();
        s.at_bottom.set(true);
        s.window_len.set(Self::WINDOW);
        s
    }

    pub fn reset_on_messages_loaded(&self, server_has_more: bool) {
        self.at_bottom.set(true);
        self.has_new_messages.set(false);
        *self.first_unread_id.borrow_mut() = None;
        self.pending_scroll_to_bottom.set(true);
        self.history_prepended_rows.set(0);
        self.window_len.set(Self::WINDOW);
        self.server_has_more.set(server_has_more);
    }

    /// Index of the first shown message in a list of `total` loaded messages.
    pub fn window_start(&self, total: usize) -> usize {
        total.saturating_sub(self.window_len.get())
    }

    /// Reveal up to `LOCAL_PAGE` older messages that are loaded but hidden.
    /// Returns how many were revealed; 0 means the server must be asked.
    pub fn reveal_local_history(&self, total: usize) -> usize {
        let hidden = self.window_start(total);
        let reveal = hidden.min(Self::LOCAL_PAGE);
        if reveal > 0 {
            self.window_len.set(self.window_len.get() + reveal);
            self.on_history_prepended(reveal);
        }
        reveal
    }

    pub fn on_viewport_at_bottom(&self, at_bottom: bool) {
        self.at_bottom.set(at_bottom);
        if at_bottom {
            self.has_new_messages.set(false);
            *self.first_unread_id.borrow_mut() = None;
        }
    }

    /// Returns true when the UI should scroll to the latest message.
    pub fn on_incoming_message(&self, is_own: bool, message_id: &str) -> bool {
        if is_own || self.at_bottom.get() {
            self.has_new_messages.set(false);
            *self.first_unread_id.borrow_mut() = None;
            self.pending_scroll_to_bottom.set(true);
            // Back at the bottom: the rows revealed while reading up go again.
            self.window_len.set(Self::WINDOW);
            true
        } else {
            // Reading older messages: keep every shown row, add the new one.
            self.window_len.set(self.window_len.get() + 1);
            self.has_new_messages.set(true);
            if self.first_unread_id.borrow().is_none() {
                *self.first_unread_id.borrow_mut() = Some(message_id.to_string());
            }
            false
        }
    }

    pub fn request_scroll_to_bottom(&self) {
        self.at_bottom.set(true);
        self.has_new_messages.set(false);
        *self.first_unread_id.borrow_mut() = None;
        self.pending_scroll_to_bottom.set(true);
    }

    /// `count` messages arrived from the server and go above the window.
    pub fn on_server_history(&self, count: usize, server_has_more: bool) {
        self.server_has_more.set(server_has_more);
        self.window_len.set(self.window_len.get() + count);
        self.on_history_prepended(count);
    }

    pub fn on_history_prepended(&self, count: usize) {
        self.history_prepended_rows.set(count as i32);
    }

    /// Put the unread rule at `message_id` and open the feed there. Called
    /// when the read cursor arrives for the crew on screen.
    pub fn open_at_unread(&self, message_id: &str) {
        self.at_bottom.set(false);
        self.has_new_messages.set(true);
        *self.first_unread_id.borrow_mut() = Some(message_id.to_string());
        self.pending_scroll_to_bottom.set(false);
        self.pending_scroll_to_unread.set(true);
    }

    pub fn apply_to_window(&self, app: &MainWindow, total: usize) {
        app.set_has_new_messages(self.has_new_messages.get());
        if self.pending_scroll_to_unread.get() {
            self.pending_scroll_to_unread.set(false);
            // The load asked for the bottom a moment ago; the rule wins.
            app.set_scroll_to_bottom_request(false);
            app.set_scroll_to_unread_request(true);
        }
        // The top of the feed asks for more while anything older exists,
        // loaded but hidden or still on the server.
        app.set_has_more_history(self.server_has_more.get() || self.window_start(total) > 0);
        if self.pending_scroll_to_bottom.get() {
            app.set_scroll_to_bottom_request(true);
        }
        let prepended = self.history_prepended_rows.get();
        if prepended > 0 {
            app.set_history_prepended_rows(prepended);
            self.history_prepended_rows.set(0);
        }
    }

    pub fn first_unread_id(&self) -> Option<String> {
        self.first_unread_id.borrow().clone()
    }
}
