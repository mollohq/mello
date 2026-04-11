use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager};

pub struct HotkeyManager {
    manager: GlobalHotKeyManager,
    ptt_hotkey: Option<HotKey>,
}

impl HotkeyManager {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            manager: GlobalHotKeyManager::new()?,
            ptt_hotkey: None,
        })
    }

    pub fn register_ptt(&mut self, hotkey: HotKey) -> Result<(), Box<dyn std::error::Error>> {
        // Unregister previous if set
        if let Some(prev) = self.ptt_hotkey.take() {
            let _ = self.manager.unregister(prev);
        }
        self.manager.register(hotkey)?;
        self.ptt_hotkey = Some(hotkey);
        Ok(())
    }

    pub fn unregister_ptt(&mut self) {
        if let Some(prev) = self.ptt_hotkey.take() {
            let _ = self.manager.unregister(prev);
        }
    }

    pub fn ptt_id(&self) -> Option<u32> {
        self.ptt_hotkey.map(|h| h.id())
    }

    /// Poll for hotkey events — call from the event loop.
    pub fn poll() -> Option<GlobalHotKeyEvent> {
        GlobalHotKeyEvent::receiver().try_recv().ok()
    }
}

/// Convert a Slint key event (key text char + modifier flags) into a
/// `global_hotkey::hotkey::HotKey` and a human-readable display label.
///
/// Returns `None` for modifier-only keys or unmappable keys.
/// Returns (HotKey, display_label, raw_hotkey_string).
/// The raw string should be stored in settings (not `HotKey::into_string()`)
/// to preserve correct modifier mapping across platforms.
pub fn slint_key_to_hotkey(
    key_text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
) -> Option<(HotKey, String, String)> {
    let c = key_text.chars().next()?;

    // Skip modifier-only keys
    match c {
        '\u{0010}' | '\u{0015}' => return None, // Shift / ShiftR
        '\u{0011}' | '\u{0016}' => return None, // Control / ControlR
        '\u{0012}' | '\u{0013}' => return None, // Alt / AltGr
        '\u{0014}' => return None,              // CapsLock
        '\u{0017}' | '\u{0018}' => return None, // Meta / MetaR
        _ => {}
    }

    let code_str = slint_char_to_code_str(c)?;

    // On macOS, Slint reports the physical Control (^) key as `meta` and
    // Option/Alt produces Unicode dead characters instead of modifier flags.
    // Swap ctrl/meta so the display and registration match the physical keys.
    #[cfg(target_os = "macos")]
    let (ctrl, meta) = (meta, ctrl);

    // Build the global-hotkey format string: "Ctrl+Shift+KeyA"
    let mut hotkey_str = String::new();
    if ctrl {
        hotkey_str.push_str("Ctrl+");
    }
    if alt {
        hotkey_str.push_str("Alt+");
    }
    if shift {
        hotkey_str.push_str("Shift+");
    }
    if meta {
        hotkey_str.push_str("Super+");
    }
    hotkey_str.push_str(code_str.as_str());

    // Parse into HotKey
    let hotkey: HotKey = hotkey_str.parse().ok()?;

    // Build display label from our constructed string, not from HotKey::into_string()
    // which can normalize modifiers differently per platform
    let label = hotkey_display_label(&hotkey_str);

    Some((hotkey, label, hotkey_str))
}

/// Convert a global-hotkey format string (e.g. from settings) back into
/// a `HotKey` and display label.
pub fn parse_hotkey_string(s: &str) -> Option<(HotKey, String)> {
    let hotkey: HotKey = s.parse().ok()?;
    let label = hotkey_display_label(s);
    Some((hotkey, label))
}

fn slint_char_to_code_str(c: char) -> Option<String> {
    let s = match c {
        // Control characters / special keys
        '\u{0008}' => "Backspace".into(),
        '\u{0009}' => "Tab".into(),
        '\u{000a}' => "Enter".into(),
        '\u{001b}' => "Escape".into(),
        '\u{007f}' => "Delete".into(),
        ' ' => "Space".into(),

        // Arrows
        '\u{F700}' => "ArrowUp".into(),
        '\u{F701}' => "ArrowDown".into(),
        '\u{F702}' => "ArrowLeft".into(),
        '\u{F703}' => "ArrowRight".into(),

        // F-keys (F1 = 0xF704 .. F24 = 0xF71B)
        '\u{F704}'..='\u{F71B}' => {
            let n = c as u32 - 0xF704 + 1;
            format!("F{n}")
        }

        // Navigation
        '\u{F727}' => "Insert".into(),
        '\u{F729}' => "Home".into(),
        '\u{F72B}' => "End".into(),
        '\u{F72C}' => "PageUp".into(),
        '\u{F72D}' => "PageDown".into(),
        '\u{F72F}' => "ScrollLock".into(),
        '\u{F730}' => "Pause".into(),

        // Letters (both cases map to KeyX)
        'a'..='z' => format!("Key{}", c.to_ascii_uppercase()),
        'A'..='Z' => format!("Key{c}"),

        // Digits
        '0'..='9' => format!("Digit{c}"),

        // Punctuation
        '`' => "Backquote".into(),
        '-' => "Minus".into(),
        '=' => "Equal".into(),
        '[' => "BracketLeft".into(),
        ']' => "BracketRight".into(),
        '\\' => "Backslash".into(),
        ';' => "Semicolon".into(),
        '\'' => "Quote".into(),
        ',' => "Comma".into(),
        '.' => "Period".into(),
        '/' => "Slash".into(),

        // Shifted digit characters → map back to digit
        '!' => "Digit1".into(),
        '@' => "Digit2".into(),
        '#' => "Digit3".into(),
        '$' => "Digit4".into(),
        '%' => "Digit5".into(),
        '^' => "Digit6".into(),
        '&' => "Digit7".into(),
        '*' => "Digit8".into(),
        '(' => "Digit9".into(),
        ')' => "Digit0".into(),

        // Shifted punctuation → map back
        '~' => "Backquote".into(),
        '_' => "Minus".into(),
        '+' => "Equal".into(),
        '{' => "BracketLeft".into(),
        '}' => "BracketRight".into(),
        '|' => "Backslash".into(),
        ':' => "Semicolon".into(),
        '"' => "Quote".into(),
        '<' => "Comma".into(),
        '>' => "Period".into(),
        '?' => "Slash".into(),

        _ => return None,
    };
    Some(s)
}

fn hotkey_display_label(hotkey_str: &str) -> String {
    hotkey_str
        .split('+')
        .map(|part| {
            match part {
                "Ctrl" | "Control" => {
                    #[cfg(target_os = "macos")]
                    {
                        "⌃"
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        "Ctrl"
                    }
                }
                "Alt" | "Option" => {
                    #[cfg(target_os = "macos")]
                    {
                        "⌥"
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        "Alt"
                    }
                }
                "Shift" => {
                    #[cfg(target_os = "macos")]
                    {
                        "⇧"
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        "Shift"
                    }
                }
                "Super" | "Cmd" | "Command" => {
                    #[cfg(target_os = "macos")]
                    {
                        "⌘"
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        "Super"
                    }
                }
                // Strip Key/Digit/Arrow prefixes for readability
                s if s.starts_with("Key") => &s[3..],
                s if s.starts_with("Digit") => &s[5..],
                s if s.starts_with("Arrow") => &s[5..],
                s => s,
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}
