use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttEvent {
    Pressed,
    Released,
}

/// Platform-agnostic key identifier for PTT and clip key matching.
/// On macOS we match via raw VK code; elsewhere via rdev::Key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PttBinding {
    pub code_str: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

/// The voice clip key of a new install (CLIPS.md section 3.1), in the
/// storage format of `Settings::clip_key`. Keybinds in Settings changes it.
pub const DEFAULT_CLIP_KEY: &str = "F7";

/// One step of the clip key: `held` is true while the key is down, so a
/// key that auto-repeats clips once. Returns true on the press that clips.
/// The modifiers must match the binding. A PTT binding on the same keys
/// keeps the keys for talking.
pub(crate) fn clip_key_step(
    held: &mut bool,
    is_clip_key: bool,
    pressed: bool,
    mods_match: bool,
    ptt_on_clip_key: bool,
) -> bool {
    if !is_clip_key {
        return false;
    }
    if !pressed {
        *held = false;
        return false;
    }
    if *held || !mods_match || ptt_on_clip_key {
        return false;
    }
    *held = true;
    true
}

/// PTT and the clip on the same keys: the keys stay with PTT. Settings
/// refuses a clip key that PTT uses, so this guards a PTT key bound later.
fn ptt_on_clip_key(ptt: &Option<PttBinding>, clip: &PttBinding) -> bool {
    ptt.as_ref() == Some(clip)
}

pub struct HotkeyManager {
    binding: Arc<Mutex<Option<PttBinding>>>,
    active: Arc<AtomicBool>,
    event_rx: mpsc::Receiver<PttEvent>,
    clip_binding: Arc<Mutex<Option<PttBinding>>>,
    clip_rx: mpsc::Receiver<()>,
}

impl HotkeyManager {
    /// A manager with no OS input listener. `poll()` always returns `None`.
    ///
    /// Used headlessly by the test harness, and as the fallback when the global
    /// listener cannot start — losing push-to-talk is a degraded experience,
    /// not a reason to refuse to launch. On macOS `start_listener` also needs
    /// Accessibility permission, which a CI runner will not have granted.
    pub fn disabled() -> Self {
        let (_tx, event_rx) = mpsc::channel();
        let (_clip_tx, clip_rx) = mpsc::channel();
        Self {
            binding: Arc::new(Mutex::new(None)),
            active: Arc::new(AtomicBool::new(false)),
            event_rx,
            clip_binding: Arc::new(Mutex::new(None)),
            clip_rx,
        }
    }

    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let binding: Arc<Mutex<Option<PttBinding>>> = Arc::new(Mutex::new(None));
        let active = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = mpsc::channel();
        let clip_binding: Arc<Mutex<Option<PttBinding>>> = Arc::new(Mutex::new(None));
        let (clip_tx, clip_rx) = mpsc::channel();

        start_listener(
            binding.clone(),
            active.clone(),
            event_tx,
            clip_binding.clone(),
            clip_tx,
        )?;

        Ok(Self {
            binding,
            active,
            event_rx,
            clip_binding,
            clip_rx,
        })
    }

    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    pub fn register_ptt(&self, binding: PttBinding) {
        *self.binding.lock().unwrap() = Some(binding);
    }

    pub fn unregister_ptt(&self) {
        *self.binding.lock().unwrap() = None;
    }

    pub fn poll(&self) -> Option<PttEvent> {
        self.event_rx.try_recv().ok()
    }

    /// The voice clip key. `None` turns the key off.
    pub fn register_clip(&self, binding: Option<PttBinding>) {
        *self.clip_binding.lock().unwrap() = binding;
    }

    /// True once for each press of the clip key since the last call. The
    /// clip key works with PTT on or off.
    pub fn poll_clip(&self) -> bool {
        self.clip_rx.try_recv().is_ok()
    }
}

// ── macOS: direct CGEventTap (avoids rdev's TSM crash on bg threads) ───────

#[cfg(target_os = "macos")]
fn start_listener(
    binding: Arc<Mutex<Option<PttBinding>>>,
    active: Arc<AtomicBool>,
    event_tx: mpsc::Sender<PttEvent>,
    clip_binding: Arc<Mutex<Option<PttBinding>>>,
    clip_tx: mpsc::Sender<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::raw::c_void;

    const K_CG_SESSION_EVENT_TAP: u32 = 1;
    const K_CG_HEAD_INSERT: u32 = 0;
    const K_CG_LISTEN_ONLY: u32 = 1;
    const K_CG_EVENT_KEY_DOWN: u64 = 10;
    const K_CG_EVENT_KEY_UP: u64 = 11;
    const K_CG_EVENT_FLAGS_CHANGED: u64 = 12;
    const K_CG_KEYBOARD_EVENT_KEYCODE: u32 = 9;

    const FLAG_SHIFT: u64 = 0x0002_0000;
    const FLAG_CONTROL: u64 = 0x0004_0000;
    const FLAG_ALT: u64 = 0x0008_0000;
    const FLAG_COMMAND: u64 = 0x0010_0000;

    extern "C" {
        fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
        fn CGEventGetFlags(event: *mut c_void) -> u64;
    }

    struct Ctx {
        binding: Arc<Mutex<Option<PttBinding>>>,
        active: Arc<AtomicBool>,
        event_tx: mpsc::Sender<PttEvent>,
        clip_binding: Arc<Mutex<Option<PttBinding>>>,
        clip_tx: mpsc::Sender<()>,
        ptt_held: bool,
        clip_held: bool,
        prev_modifier_flags: u64,
    }

    unsafe extern "C" fn tap_callback(
        _proxy: *mut c_void,
        event_type: u32,
        event: *mut c_void,
        user_info: *mut c_void,
    ) -> *mut c_void {
        let ctx = &mut *(user_info as *mut Ctx);

        // The clip key comes before the PTT gate: it works with PTT off.
        let ev = event_type as u64;
        let clip = ctx.clip_binding.lock().unwrap().clone();
        if let (Some(clip), true) = (clip, ev == K_CG_EVENT_KEY_DOWN || ev == K_CG_EVENT_KEY_UP) {
            let vk = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) as u16;
            let flags = CGEventGetFlags(event);
            let mods_match = (flags & FLAG_CONTROL != 0) == clip.ctrl
                && (flags & FLAG_ALT != 0) == clip.alt
                && (flags & FLAG_SHIFT != 0) == clip.shift
                && (flags & FLAG_COMMAND != 0) == clip.meta;
            let ptt_on_clip = ptt_on_clip_key(&ctx.binding.lock().unwrap(), &clip)
                && ctx.active.load(Ordering::Relaxed);
            if clip_key_step(
                &mut ctx.clip_held,
                Some(vk) == code_str_to_macos_vk(&clip.code_str),
                ev == K_CG_EVENT_KEY_DOWN,
                mods_match,
                ptt_on_clip,
            ) {
                log::info!("[hotkeys] clip key pressed");
                let _ = ctx.clip_tx.send(());
            }
        }

        if !ctx.active.load(Ordering::Relaxed) {
            if ctx.ptt_held {
                ctx.ptt_held = false;
                let _ = ctx.event_tx.send(PttEvent::Released);
            }
            ctx.prev_modifier_flags = 0;
            return event;
        }

        let binding_guard = ctx.binding.lock().unwrap();
        let Some(ref binding) = *binding_guard else {
            return event;
        };

        let vk = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) as u16;
        let flags = CGEventGetFlags(event);
        let ctrl_held = flags & FLAG_CONTROL != 0;
        let alt_held = flags & FLAG_ALT != 0;
        let shift_held = flags & FLAG_SHIFT != 0;
        let meta_held = flags & FLAG_COMMAND != 0;

        let mods_match = ctrl_held == binding.ctrl
            && alt_held == binding.alt
            && shift_held == binding.shift
            && meta_held == binding.meta;

        let target_vk = code_str_to_macos_vk(&binding.code_str);

        match event_type as u64 {
            K_CG_EVENT_KEY_DOWN => {
                if let Some(tvk) = target_vk {
                    if vk == tvk && mods_match && !ctx.ptt_held {
                        ctx.ptt_held = true;
                        log::info!("[hotkeys] PTT pressed (vk={})", vk);
                        let _ = ctx.event_tx.send(PttEvent::Pressed);
                    }
                }
            }
            K_CG_EVENT_KEY_UP => {
                if let Some(tvk) = target_vk {
                    if ctx.ptt_held && vk == tvk {
                        ctx.ptt_held = false;
                        log::info!("[hotkeys] PTT released (vk={})", vk);
                        let _ = ctx.event_tx.send(PttEvent::Released);
                    }
                }
            }
            K_CG_EVENT_FLAGS_CHANGED => {
                // Modifier-only: if target is a modifier key, detect press/release
                // via flag changes. Also release PTT if a required modifier is lifted.
                if ctx.ptt_held && !mods_match {
                    ctx.ptt_held = false;
                    log::info!("[hotkeys] PTT released (modifier lifted)");
                    let _ = ctx.event_tx.send(PttEvent::Released);
                }
                ctx.prev_modifier_flags = flags;
            }
            _ => {}
        }

        event
    }

    let event_mask: u64 =
        (1 << K_CG_EVENT_KEY_DOWN) | (1 << K_CG_EVENT_KEY_UP) | (1 << K_CG_EVENT_FLAGS_CHANGED);

    let ctx = Box::new(Ctx {
        binding,
        active,
        event_tx,
        clip_binding,
        clip_tx,
        ptt_held: false,
        clip_held: false,
        prev_modifier_flags: 0,
    });
    // Safety: Ctx only contains Send types (Arc, mpsc::Sender, bool, u64).
    // We transmit the pointer as usize to satisfy Send, then cast back on
    // the listener thread which owns it exclusively.
    let ctx_addr = Box::into_raw(ctx) as usize;

    std::thread::Builder::new()
        .name("ptt-listener".into())
        .spawn(move || {
            log::info!("[hotkeys] macOS CGEventTap listener starting");
            let ctx_ptr = ctx_addr as *mut c_void;

            type CGEventTapCallBack =
                unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;
            extern "C" {
                fn CGEventTapCreate(
                    tap: u32,
                    place: u32,
                    options: u32,
                    events_of_interest: u64,
                    callback: CGEventTapCallBack,
                    user_info: *mut c_void,
                ) -> *mut c_void;
                fn CFMachPortCreateRunLoopSource(
                    allocator: *const c_void,
                    port: *mut c_void,
                    order: i64,
                ) -> *mut c_void;
                fn CFRunLoopGetCurrent() -> *mut c_void;
                fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
                fn CFRunLoopRun();
                static kCFRunLoopCommonModes: *const c_void;
            }

            unsafe {
                let tap = CGEventTapCreate(
                    K_CG_SESSION_EVENT_TAP,
                    K_CG_HEAD_INSERT,
                    K_CG_LISTEN_ONLY,
                    event_mask,
                    tap_callback,
                    ctx_ptr,
                );
                if tap.is_null() {
                    log::error!(
                        "[hotkeys] CGEventTapCreate failed — grant Input Monitoring \
                         permission in System Settings → Privacy & Security"
                    );
                    return;
                }
                let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
                let rl = CFRunLoopGetCurrent();
                CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
                log::info!("[hotkeys] macOS CGEventTap listener running");
                CFRunLoopRun();
            }
            log::warn!("[hotkeys] macOS listener thread exited");
        })?;

    Ok(())
}

#[cfg(target_os = "macos")]
fn code_str_to_macos_vk(s: &str) -> Option<u16> {
    Some(match s {
        "KeyA" => 0x00,
        "KeyS" => 0x01,
        "KeyD" => 0x02,
        "KeyF" => 0x03,
        "KeyH" => 0x04,
        "KeyG" => 0x05,
        "KeyZ" => 0x06,
        "KeyX" => 0x07,
        "KeyC" => 0x08,
        "KeyV" => 0x09,
        "KeyB" => 0x0B,
        "KeyQ" => 0x0C,
        "KeyW" => 0x0D,
        "KeyE" => 0x0E,
        "KeyR" => 0x0F,
        "KeyY" => 0x10,
        "KeyT" => 0x11,
        "KeyO" => 0x1F,
        "KeyU" => 0x20,
        "KeyI" => 0x22,
        "KeyP" => 0x23,
        "KeyL" => 0x25,
        "KeyJ" => 0x26,
        "KeyK" => 0x28,
        "KeyN" => 0x2D,
        "KeyM" => 0x2E,
        "Digit1" => 0x12,
        "Digit2" => 0x13,
        "Digit3" => 0x14,
        "Digit4" => 0x15,
        "Digit5" => 0x17,
        "Digit6" => 0x16,
        "Digit7" => 0x1A,
        "Digit8" => 0x1C,
        "Digit9" => 0x19,
        "Digit0" => 0x1D,
        "Equal" => 0x18,
        "Minus" => 0x1B,
        "BracketRight" => 0x1E,
        "BracketLeft" => 0x21,
        "Quote" => 0x27,
        "Semicolon" => 0x29,
        "Backslash" => 0x2A,
        "Comma" => 0x2B,
        "Slash" => 0x2C,
        "Period" => 0x2F,
        "Backquote" => 0x32,
        "Enter" => 0x24,
        "Tab" => 0x30,
        "Space" => 0x31,
        "Backspace" => 0x33,
        "Escape" => 0x35,
        "Delete" => 0x75,
        "CapsLock" => 0x39,
        "F1" => 0x7A,
        "F2" => 0x78,
        "F3" => 0x63,
        "F4" => 0x76,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F8" => 0x64,
        "F9" => 0x65,
        "F10" => 0x6D,
        "F11" => 0x67,
        "F12" => 0x6F,
        "Home" => 0x73,
        "End" => 0x77,
        "PageUp" => 0x74,
        "PageDown" => 0x79,
        "ArrowLeft" => 0x7B,
        "ArrowRight" => 0x7C,
        "ArrowDown" => 0x7D,
        "ArrowUp" => 0x7E,
        _ => return None,
    })
}

// ── Non-macOS: rdev listener ───────────────────────────────────────────────

#[cfg(not(target_os = "macos"))]
fn start_listener(
    binding: Arc<Mutex<Option<PttBinding>>>,
    active: Arc<AtomicBool>,
    event_tx: mpsc::Sender<PttEvent>,
    clip_binding: Arc<Mutex<Option<PttBinding>>>,
    clip_tx: mpsc::Sender<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    use rdev::{self, EventType, Key};

    #[derive(Default)]
    struct ModifierState {
        ctrl: bool,
        alt: bool,
        shift: bool,
        meta: bool,
    }

    impl ModifierState {
        fn update(&mut self, key: &Key, pressed: bool) {
            match key {
                Key::ControlLeft | Key::ControlRight => self.ctrl = pressed,
                Key::Alt | Key::AltGr => self.alt = pressed,
                Key::ShiftLeft | Key::ShiftRight => self.shift = pressed,
                Key::MetaLeft | Key::MetaRight => self.meta = pressed,
                _ => {}
            }
        }

        fn matches(&self, binding: &PttBinding) -> bool {
            self.ctrl == binding.ctrl
                && self.alt == binding.alt
                && self.shift == binding.shift
                && self.meta == binding.meta
        }
    }

    fn code_str_to_rdev_key(s: &str) -> Option<Key> {
        Some(match s {
            "KeyA" => Key::KeyA,
            "KeyB" => Key::KeyB,
            "KeyC" => Key::KeyC,
            "KeyD" => Key::KeyD,
            "KeyE" => Key::KeyE,
            "KeyF" => Key::KeyF,
            "KeyG" => Key::KeyG,
            "KeyH" => Key::KeyH,
            "KeyI" => Key::KeyI,
            "KeyJ" => Key::KeyJ,
            "KeyK" => Key::KeyK,
            "KeyL" => Key::KeyL,
            "KeyM" => Key::KeyM,
            "KeyN" => Key::KeyN,
            "KeyO" => Key::KeyO,
            "KeyP" => Key::KeyP,
            "KeyQ" => Key::KeyQ,
            "KeyR" => Key::KeyR,
            "KeyS" => Key::KeyS,
            "KeyT" => Key::KeyT,
            "KeyU" => Key::KeyU,
            "KeyV" => Key::KeyV,
            "KeyW" => Key::KeyW,
            "KeyX" => Key::KeyX,
            "KeyY" => Key::KeyY,
            "KeyZ" => Key::KeyZ,
            "Digit0" => Key::Num0,
            "Digit1" => Key::Num1,
            "Digit2" => Key::Num2,
            "Digit3" => Key::Num3,
            "Digit4" => Key::Num4,
            "Digit5" => Key::Num5,
            "Digit6" => Key::Num6,
            "Digit7" => Key::Num7,
            "Digit8" => Key::Num8,
            "Digit9" => Key::Num9,
            "F1" => Key::F1,
            "F2" => Key::F2,
            "F3" => Key::F3,
            "F4" => Key::F4,
            "F5" => Key::F5,
            "F6" => Key::F6,
            "F7" => Key::F7,
            "F8" => Key::F8,
            "F9" => Key::F9,
            "F10" => Key::F10,
            "F11" => Key::F11,
            "F12" => Key::F12,
            "Space" => Key::Space,
            "Tab" => Key::Tab,
            "Enter" => Key::Return,
            "Backspace" => Key::Backspace,
            "Delete" => Key::Delete,
            "Escape" => Key::Escape,
            "ArrowUp" => Key::UpArrow,
            "ArrowDown" => Key::DownArrow,
            "ArrowLeft" => Key::LeftArrow,
            "ArrowRight" => Key::RightArrow,
            "Home" => Key::Home,
            "End" => Key::End,
            "PageUp" => Key::PageUp,
            "PageDown" => Key::PageDown,
            "Insert" => Key::Insert,
            "CapsLock" => Key::CapsLock,
            "ScrollLock" => Key::ScrollLock,
            "Pause" => Key::Pause,
            "Backquote" => Key::BackQuote,
            "Minus" => Key::Minus,
            "Equal" => Key::Equal,
            "BracketLeft" => Key::LeftBracket,
            "BracketRight" => Key::RightBracket,
            "Backslash" => Key::BackSlash,
            "Semicolon" => Key::SemiColon,
            "Quote" => Key::Quote,
            "Comma" => Key::Comma,
            "Period" => Key::Dot,
            "Slash" => Key::Slash,
            _ => return None,
        })
    }

    std::thread::Builder::new()
        .name("ptt-listener".into())
        .spawn(move || {
            log::info!("[hotkeys] rdev listener thread started");
            let mut mods = ModifierState::default();
            let mut ptt_held = false;
            // The PTT state resets while PTT is off; the clip key keeps its own.
            let mut clip_mods = ModifierState::default();
            let mut clip_held = false;

            if let Err(e) = rdev::listen(move |event| {
                let (key, pressed) = match event.event_type {
                    EventType::KeyPress(k) => (k, true),
                    EventType::KeyRelease(k) => (k, false),
                    _ => return,
                };

                // The clip key comes before the PTT gate: it works with PTT off.
                clip_mods.update(&key, pressed);
                let clip = clip_binding.lock().unwrap().clone();
                if let Some(clip) = clip {
                    let ptt_on_clip = ptt_on_clip_key(&binding.lock().unwrap(), &clip)
                        && active.load(Ordering::Relaxed);
                    if clip_key_step(
                        &mut clip_held,
                        Some(key) == code_str_to_rdev_key(&clip.code_str),
                        pressed,
                        clip_mods.matches(&clip),
                        ptt_on_clip,
                    ) {
                        log::info!("[hotkeys] clip key pressed");
                        let _ = clip_tx.send(());
                    }
                }

                if !active.load(Ordering::Relaxed) {
                    mods = ModifierState::default();
                    if ptt_held {
                        ptt_held = false;
                        let _ = event_tx.send(PttEvent::Released);
                    }
                    return;
                }

                let binding_guard = binding.lock().unwrap();
                let Some(ref b) = *binding_guard else {
                    return;
                };
                let Some(target_key) = code_str_to_rdev_key(&b.code_str) else {
                    return;
                };

                match event.event_type {
                    EventType::KeyPress(key) => {
                        mods.update(&key, true);
                        if key == target_key && mods.matches(b) && !ptt_held {
                            ptt_held = true;
                            log::debug!("[hotkeys] PTT pressed");
                            let _ = event_tx.send(PttEvent::Pressed);
                        }
                    }
                    EventType::KeyRelease(key) => {
                        mods.update(&key, false);
                        if ptt_held && (key == target_key || !mods.matches(b)) {
                            ptt_held = false;
                            log::debug!("[hotkeys] PTT released");
                            let _ = event_tx.send(PttEvent::Released);
                        }
                    }
                    _ => {}
                }
            }) {
                log::error!("[hotkeys] rdev listener failed: {:?}", e);
            }
            log::warn!("[hotkeys] listener thread exited");
        })?;

    Ok(())
}

// ── Key mapping (Slint → storage string) ───────────────────────────────────

pub fn slint_key_to_ptt(
    key_text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
) -> Option<(PttBinding, String, String)> {
    let c = key_text.chars().next()?;

    match c {
        '\u{0010}' | '\u{0015}' => return None,
        '\u{0011}' | '\u{0016}' => return None,
        '\u{0012}' | '\u{0013}' => return None,
        '\u{0014}' => return None,
        '\u{0017}' | '\u{0018}' => return None,
        _ => {}
    }

    let code_str = slint_char_to_code_str(c)?;

    #[cfg(target_os = "macos")]
    let (ctrl, meta) = (meta, ctrl);

    let mut storage = String::new();
    if ctrl {
        storage.push_str("Ctrl+");
    }
    if alt {
        storage.push_str("Alt+");
    }
    if shift {
        storage.push_str("Shift+");
    }
    if meta {
        storage.push_str("Super+");
    }
    storage.push_str(&code_str);

    let label = hotkey_display_label(&storage);

    let binding = PttBinding {
        code_str,
        ctrl,
        alt,
        shift,
        meta,
    };

    Some((binding, label, storage))
}

pub fn parse_ptt_string(s: &str) -> Option<(PttBinding, String)> {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut meta = false;
    let mut key_part = s;

    for prefix in ["Ctrl+", "Alt+", "Shift+", "Super+"] {
        if let Some(rest) = key_part.strip_prefix(prefix) {
            match prefix {
                "Ctrl+" => ctrl = true,
                "Alt+" => alt = true,
                "Shift+" => shift = true,
                "Super+" => meta = true,
                _ => {}
            }
            key_part = rest;
        }
    }

    let label = hotkey_display_label(s);

    Some((
        PttBinding {
            code_str: key_part.to_string(),
            ctrl,
            alt,
            shift,
            meta,
        },
        label,
    ))
}

fn slint_char_to_code_str(c: char) -> Option<String> {
    let s = match c {
        '\u{0008}' => "Backspace".into(),
        '\u{0009}' => "Tab".into(),
        '\u{000a}' => "Enter".into(),
        '\u{001b}' => "Escape".into(),
        '\u{007f}' => "Delete".into(),
        ' ' => "Space".into(),
        '\u{F700}' => "ArrowUp".into(),
        '\u{F701}' => "ArrowDown".into(),
        '\u{F702}' => "ArrowLeft".into(),
        '\u{F703}' => "ArrowRight".into(),
        '\u{F704}'..='\u{F71B}' => {
            let n = c as u32 - 0xF704 + 1;
            format!("F{n}")
        }
        '\u{F727}' => "Insert".into(),
        '\u{F729}' => "Home".into(),
        '\u{F72B}' => "End".into(),
        '\u{F72C}' => "PageUp".into(),
        '\u{F72D}' => "PageDown".into(),
        '\u{F72F}' => "ScrollLock".into(),
        '\u{F730}' => "Pause".into(),
        'a'..='z' => format!("Key{}", c.to_ascii_uppercase()),
        'A'..='Z' => format!("Key{c}"),
        '0'..='9' => format!("Digit{c}"),
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
        .map(|part| match part {
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
            s if s.starts_with("Key") => &s[3..],
            s if s.starts_with("Digit") => &s[5..],
            s if s.starts_with("Arrow") => &s[5..],
            s => s,
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds key events through `clip_key_step` and counts the clips.
    /// An event is (is the clip key, pressed, the modifiers match).
    fn clips(events: &[(bool, bool, bool)], ptt_on_clip_key: bool) -> usize {
        let mut held = false;
        events
            .iter()
            .filter(|&&(is_clip, pressed, mods_match)| {
                clip_key_step(&mut held, is_clip, pressed, mods_match, ptt_on_clip_key)
            })
            .count()
    }

    #[test]
    fn a_press_of_the_clip_key_clips_once_even_when_it_repeats() {
        // Down, two auto-repeats, up: one clip.
        let held_down = [
            (true, true, true),
            (true, true, true),
            (true, true, true),
            (true, false, true),
        ];
        assert_eq!(clips(&held_down, false), 1);
        // Two separate presses: two clips.
        let twice = [
            (true, true, true),
            (true, false, true),
            (true, true, true),
            (true, false, true),
        ];
        assert_eq!(clips(&twice, false), 2);
    }

    #[test]
    fn other_keys_and_other_modifiers_do_not_clip() {
        assert_eq!(
            clips(&[(false, true, true), (false, false, true)], false),
            0
        );
        assert_eq!(
            clips(&[(true, true, false), (true, false, false)], false),
            0
        );
    }

    #[test]
    fn a_ptt_binding_on_the_clip_keys_keeps_the_keys_for_talking() {
        assert_eq!(clips(&[(true, true, true), (true, false, true)], true), 0);
        let (f7, _) = parse_ptt_string("F7").expect("F7");
        let (ctrl_f7, _) = parse_ptt_string("Ctrl+F7").expect("Ctrl+F7");
        assert!(ptt_on_clip_key(&Some(f7.clone()), &f7));
        assert!(!ptt_on_clip_key(&Some(ctrl_f7), &f7));
        assert!(!ptt_on_clip_key(&None, &f7));
    }

    #[test]
    fn the_default_clip_key_parses() {
        let (b, label) = parse_ptt_string(DEFAULT_CLIP_KEY).expect("default clip key");
        assert_eq!(b.code_str, "F7");
        assert!(!(b.ctrl || b.alt || b.shift || b.meta));
        assert_eq!(label, "F7");
    }
}
