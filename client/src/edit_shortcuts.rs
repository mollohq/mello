//! The Edit menu items (Undo, Redo, Cut, Copy, Paste, Select All) and the key
//! presses they forward to the Slint window.
//!
//! # Why the menu forwards keys
//!
//! On macOS the menu bar receives a key equivalent (Cmd+C, Cmd+V, ...) before
//! the window does. muda's `PredefinedMenuItem::copy()` and the others do not
//! act by themselves. They send the Cocoa action `copy:` to the first
//! responder. The first responder is winit's `WinitView`, and it implements
//! none of these actions (winit 0.30.13, `platform_impl/macos/view.rs`).
//! So the menu consumed the shortcut, nothing happened, and the Slint
//! `TextInput` never saw the key.
//!
//! Our own `MenuItem`s fix this. When one fires, from the menu or from its
//! shortcut, [`forward`] sends the same key press to the Slint window that
//! the window would have received without the menu.
//!
//! # What Slint expects for the Command key
//!
//! Slint maps the Command key to its `Control` key and does not use its `Meta`
//! key for it:
//!
//! - `i-slint-backend-winit-1.17.0/event_loop.rs`, `WindowEvent::KeyboardInput`
//!   (about line 258): on Apple platforms winit's `Control` is swapped with
//!   `Super`. A physical Command key becomes `NamedKey::Control`, and that key
//!   becomes the Slint key code `Key::Control` (U+0011, `i-slint-common`
//!   `key_codes.rs`).
//! - `i-slint-core-1.17.0/input.rs`, `InternalKeyboardModifierState`
//!   (about line 316): `Key::Control` sets `left_control`, and
//!   `KeyboardModifiers::control` reads it. `Key::Meta` sets `meta`, which is
//!   the physical Control key on macOS.
//! - `i-slint-core-1.17.0/input.rs`, `InternalKeyEvent::shortcut()`
//!   (about line 972): `TextInput` shortcuts check `modifiers.control`, not
//!   `meta`. Control alone gives Copy ("c"), Cut ("x"), Paste ("v"),
//!   Select All ("a") and Undo ("z"). Control and Shift give Redo ("z" or
//!   "Z"). Slint enables Redo on Control+Shift+Z on every target except
//!   Windows.
//! - `i-slint-core-1.17.0/items/text.rs`, `TextInput::key_event`
//!   (about line 1026): `shortcut()` selects `copy`, `paste`, `cut`,
//!   `select_all`, `undo` and `redo`.
//!
//! So [`key_sequence`] uses `Key::Control` for Command on every platform.
//!
//! # Known limit
//!
//! Slint tracks the modifier state from key events only, and it has one flag
//! for Command. A real Command key press sets the flag. Our forwarded release
//! of `Key::Control` clears it while the user still holds Command. A second
//! Command shortcut that has no menu item, for example Cmd+Left, then needs a
//! new Command key press. Menu items keep working, because the menu handles
//! them and we send a new press each time.

// The functions have callers on macOS only (the menu and the poll loop).
// Tests use them on every platform.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use slint::platform::{Key, WindowEvent};
use slint::SharedString;

pub const UNDO: &str = "edit.undo";
pub const REDO: &str = "edit.redo";
pub const CUT: &str = "edit.cut";
pub const COPY: &str = "edit.copy";
pub const PASTE: &str = "edit.paste";
pub const SELECT_ALL: &str = "edit.select_all";

/// One Edit menu item: what the menu shows and the key it forwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditItem {
    pub id: &'static str,
    pub title: &'static str,
    /// The key equivalent, in the muda accelerator syntax.
    pub accelerator: &'static str,
    /// The letter key that Slint's `TextInput` reads with the Command key.
    pub letter: char,
    /// Whether Shift is part of the shortcut.
    pub shift: bool,
}

/// The items in menu order. The menu bar builds its items from this table.
pub const EDIT_ITEMS: [EditItem; 6] = [
    EditItem {
        id: UNDO,
        title: "Undo",
        accelerator: "cmd+z",
        letter: 'z',
        shift: false,
    },
    EditItem {
        id: REDO,
        title: "Redo",
        accelerator: "cmd+shift+z",
        // With Shift held, winit reports the upper-case letter.
        letter: 'Z',
        shift: true,
    },
    EditItem {
        id: CUT,
        title: "Cut",
        accelerator: "cmd+x",
        letter: 'x',
        shift: false,
    },
    EditItem {
        id: COPY,
        title: "Copy",
        accelerator: "cmd+c",
        letter: 'c',
        shift: false,
    },
    EditItem {
        id: PASTE,
        title: "Paste",
        accelerator: "cmd+v",
        letter: 'v',
        shift: false,
    },
    EditItem {
        id: SELECT_ALL,
        title: "Select All",
        accelerator: "cmd+a",
        letter: 'a',
        shift: false,
    },
];

/// One key event to send to the Slint window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStep {
    Press(char),
    Release(char),
}

impl KeyStep {
    fn to_window_event(self) -> WindowEvent {
        match self {
            KeyStep::Press(c) => WindowEvent::KeyPressed {
                text: SharedString::from(c),
            },
            KeyStep::Release(c) => WindowEvent::KeyReleased {
                text: SharedString::from(c),
            },
        }
    }
}

/// True when `menu_id` is one of the Edit menu items in [`EDIT_ITEMS`].
pub fn is_edit_item(menu_id: &str) -> bool {
    EDIT_ITEMS.iter().any(|item| item.id == menu_id)
}

/// The key events that Slint receives for the shortcut of this menu item.
///
/// It presses the modifiers, presses and releases the letter, then releases
/// the modifiers in reverse order. Command is `Key::Control` (see the module
/// documentation). Returns `None` for an id that is not an Edit item.
pub fn key_sequence(menu_id: &str) -> Option<Vec<KeyStep>> {
    let item = EDIT_ITEMS.iter().find(|item| item.id == menu_id)?;
    let command: char = Key::Control.into();
    let shift: char = Key::Shift.into();

    let mut steps = vec![KeyStep::Press(command)];
    if item.shift {
        steps.push(KeyStep::Press(shift));
    }
    steps.push(KeyStep::Press(item.letter));
    steps.push(KeyStep::Release(item.letter));
    if item.shift {
        steps.push(KeyStep::Release(shift));
    }
    steps.push(KeyStep::Release(command));
    Some(steps)
}

/// Send the shortcut of this menu item to the window.
///
/// The focused Slint text field handles it as if the key had reached it.
/// Returns `false`, and sends nothing, when `menu_id` is not an Edit item.
pub fn forward(window: &slint::Window, menu_id: &str) -> bool {
    let Some(steps) = key_sequence(menu_id) else {
        return false;
    };
    for step in steps {
        window.dispatch_event(step.to_window_event());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: char = '\u{11}'; // Key::Control: the Command key on macOS
    const SHIFT: char = '\u{10}'; // Key::Shift

    #[test]
    fn a_plain_shortcut_presses_command_then_the_letter() {
        assert_eq!(
            key_sequence(COPY).unwrap(),
            vec![
                KeyStep::Press(CMD),
                KeyStep::Press('c'),
                KeyStep::Release('c'),
                KeyStep::Release(CMD),
            ]
        );
    }

    #[test]
    fn redo_holds_shift_and_sends_the_upper_case_letter() {
        assert_eq!(
            key_sequence(REDO).unwrap(),
            vec![
                KeyStep::Press(CMD),
                KeyStep::Press(SHIFT),
                KeyStep::Press('Z'),
                KeyStep::Release('Z'),
                KeyStep::Release(SHIFT),
                KeyStep::Release(CMD),
            ]
        );
    }

    #[test]
    fn each_item_maps_to_its_letter() {
        let letter = |id| match key_sequence(id).unwrap()[1] {
            KeyStep::Press(c) => c,
            other => panic!("expected a letter press, got {other:?}"),
        };
        assert_eq!(letter(UNDO), 'z');
        assert_eq!(letter(CUT), 'x');
        assert_eq!(letter(COPY), 'c');
        assert_eq!(letter(PASTE), 'v');
        assert_eq!(letter(SELECT_ALL), 'a');
    }

    #[test]
    fn every_press_is_released_in_reverse_order() {
        for item in EDIT_ITEMS {
            let steps = key_sequence(item.id).unwrap();
            let presses: Vec<char> = steps
                .iter()
                .filter_map(|s| match s {
                    KeyStep::Press(c) => Some(*c),
                    _ => None,
                })
                .collect();
            let mut releases: Vec<char> = steps
                .iter()
                .filter_map(|s| match s {
                    KeyStep::Release(c) => Some(*c),
                    _ => None,
                })
                .collect();
            releases.reverse();
            assert_eq!(presses, releases, "{}: unbalanced key events", item.id);
        }
    }

    #[test]
    fn unknown_ids_have_no_sequence() {
        assert_eq!(key_sequence("find"), None);
        assert_eq!(key_sequence("prefs"), None);
        assert_eq!(key_sequence("edit.nothing"), None);
        assert_eq!(key_sequence(""), None);
        assert!(!is_edit_item("find"));
        assert!(is_edit_item(PASTE));
    }

    #[test]
    fn the_menu_table_is_consistent() {
        let mut ids: Vec<_> = EDIT_ITEMS.iter().map(|i| i.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), EDIT_ITEMS.len(), "duplicate menu id");
        for item in EDIT_ITEMS {
            // The accelerator names the same letter and modifiers that we forward.
            assert!(
                item.accelerator.starts_with("cmd+"),
                "{}: the accelerator must use Command",
                item.id
            );
            assert_eq!(
                item.accelerator.contains("shift"),
                item.shift,
                "{}: the shift flag disagrees with the accelerator",
                item.id
            );
            assert!(
                item.accelerator
                    .ends_with(&item.letter.to_ascii_lowercase().to_string()),
                "{}: the accelerator and the forwarded letter differ",
                item.id
            );
        }
    }
}
