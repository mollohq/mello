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
    /// Estimated rows prepended on last history load (for viewport nudge).
    pub history_prepended_rows: Cell<i32>,
}

impl ChatScrollState {
    pub fn new() -> Self {
        let s = Self::default();
        s.at_bottom.set(true);
        s
    }

    pub fn reset_on_messages_loaded(&self) {
        self.at_bottom.set(true);
        self.has_new_messages.set(false);
        *self.first_unread_id.borrow_mut() = None;
        self.pending_scroll_to_bottom.set(true);
        self.history_prepended_rows.set(0);
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
            true
        } else {
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

    pub fn on_history_prepended(&self, count: usize) {
        self.history_prepended_rows.set(count as i32);
    }

    pub fn apply_to_window(&self, app: &MainWindow) {
        app.set_has_new_messages(self.has_new_messages.get());
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
